use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use toml_edit::DocumentMut;

use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::Provider;
use crate::store::AppState;

use super::gemini_auth::{detect_gemini_auth_type, GeminiAuthType};
use super::live::{build_effective_settings_with_common_config, sanitize_claude_settings_for_live};

pub(super) const CLAUDE_SETTINGS_PATH: &str = "$HOME/.claude/settings.json";
pub(super) const CODEX_AUTH_PATH: &str = "$HOME/.codex/auth.json";
pub(super) const CODEX_CONFIG_PATH: &str = "$HOME/.codex/config.toml";
pub(super) const GEMINI_ENV_PATH: &str = "$HOME/.gemini/.env";
pub(super) const GEMINI_SETTINGS_PATH: &str = "$HOME/.gemini/settings.json";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SshHostEntry {
    pub alias: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnectionTarget {
    #[serde(default, rename = "type")]
    pub target_type: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedSshTarget {
    pub(super) label: String,
    pub(super) connect_target: String,
    pub(super) port: Option<u16>,
    pub(super) password: Option<String>,
}

impl ResolvedSshTarget {
    pub(super) fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteApplyResult {
    pub host_alias: String,
    pub app: String,
    pub provider_id: String,
    pub written_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub overwrote_existing_config: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Remote state after the write, so callers don't need another round trip.
    pub remote_state: RemoteProviderState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfigFile {
    pub path: String,
    pub exists: bool,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProviderState {
    pub host_alias: String,
    pub app: String,
    pub provider: Option<Provider>,
    pub matched_provider_id: Option<String>,
    pub files: Vec<RemoteConfigFile>,
    pub has_existing_config: bool,
    pub has_unmanaged_config: bool,
    pub overwrite_warning: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The remote CLI points at the local gateway through the SSH tunnel.
    #[serde(default)]
    pub via_gateway: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImportResult {
    pub host_alias: String,
    pub app: String,
    pub provider: Provider,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProcessInfo {
    pub pid: u32,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRestartResult {
    pub host_alias: String,
    pub app: String,
    pub stopped: Vec<RemoteProcessInfo>,
    pub force_killed: Vec<u32>,
}

/// Contents of the remote live config files for one app; `None` means missing.
pub(super) struct RemoteSnapshot {
    pub(super) files: Vec<(&'static str, Option<String>)>,
}

impl RemoteSnapshot {
    pub(super) fn get(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(file_path, _)| *file_path == path)
            .and_then(|(_, content)| content.as_deref())
    }

    pub(super) fn has_any(&self) -> bool {
        self.files.iter().any(|(_, content)| content.is_some())
    }

    fn file_statuses(&self) -> Vec<RemoteConfigFile> {
        self.files
            .iter()
            .map(|(path, content)| RemoteConfigFile {
                path: path.to_string(),
                exists: content.is_some(),
                bytes: content.as_ref().map_or(0, String::len),
            })
            .collect()
    }
}

/// `content: None` removes the remote file.
pub(super) struct RemoteWrite {
    pub(super) path: &'static str,
    pub(super) content: Option<String>,
}

struct RemoteSettingsRead {
    settings_config: Option<Value>,
    warnings: Vec<String>,
}

pub struct RemoteProviderService;

impl RemoteProviderService {
    pub fn list_ssh_hosts() -> Result<Vec<SshHostEntry>, AppError> {
        let config_path = crate::config::get_home_dir().join(".ssh").join("config");
        let mut hosts = Vec::new();
        let mut index = HashMap::new();
        let mut visited = HashSet::new();
        parse_ssh_config_file(&config_path, &mut hosts, &mut index, &mut visited)?;
        Ok(hosts)
    }

    pub fn apply_provider_to_remote(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
        target: &SshConnectionTarget,
        force_overwrite: bool,
    ) -> Result<RemoteApplyResult, AppError> {
        ensure_remote_supported(&app_type)?;
        let target = resolve_ssh_target(target)?;
        let host_alias = target.label();
        let _in_flight = InFlightGuard::acquire(format!("{host_alias}\n{}", app_type.as_str()))
            .ok_or_else(|| AppError::Message(format!("远端 {host_alias} 正在切换中，请稍候")))?;

        let providers = state.db.get_all_providers(app_type.as_str())?;
        let provider = providers
            .get(provider_id)
            .ok_or_else(|| AppError::Message(format!("供应商 {provider_id} 不存在")))?;

        let snapshot = read_remote_snapshot(&app_type, &target)?;
        let has_existing_config = snapshot.has_any();
        if has_existing_config
            && !force_overwrite
            && !super::remote_gateway::snapshot_uses_gateway(&state.db, host_alias, &snapshot)
        {
            let remote_settings = remote_settings_from_snapshot(&app_type, &snapshot)?;
            let matched = find_matching_local_provider(
                state,
                &app_type,
                &snapshot,
                remote_settings.settings_config.as_ref(),
            )?;
            if matched.is_none() {
                return Err(AppError::Message(remote_overwrite_block_message(
                    &app_type, host_alias,
                )));
            }
        }

        let writes: Vec<RemoteWrite> = build_remote_writes(state, &app_type, provider, &snapshot)?
            .into_iter()
            .filter(|write| write.content.is_some() || snapshot.get(write.path).is_some())
            .collect();
        let (written_files, removed_files) = apply_remote_writes(&target, &writes)?;

        let mut after = snapshot;
        for write in writes {
            if let Some((_, content)) = after.files.iter_mut().find(|(path, _)| *path == write.path)
            {
                *content = write.content;
            }
        }
        let remote_state = remote_state_from_snapshot(state, &app_type, host_alias, &after)?;

        Ok(RemoteApplyResult {
            host_alias: host_alias.to_string(),
            app: app_type.as_str().to_string(),
            provider_id: provider_id.to_string(),
            written_files,
            removed_files,
            overwrote_existing_config: has_existing_config,
            warnings: Vec::new(),
            remote_state,
        })
    }

    pub fn inspect_remote_provider(
        state: &AppState,
        app_type: AppType,
        target: &SshConnectionTarget,
    ) -> Result<RemoteProviderState, AppError> {
        ensure_remote_supported(&app_type)?;
        let target = resolve_ssh_target(target)?;
        let snapshot = read_remote_snapshot(&app_type, &target)?;
        remote_state_from_snapshot(state, &app_type, target.label(), &snapshot)
    }

    pub fn import_remote_provider(
        state: &AppState,
        app_type: AppType,
        target: &SshConnectionTarget,
    ) -> Result<RemoteImportResult, AppError> {
        ensure_remote_supported(&app_type)?;
        let target = resolve_ssh_target(target)?;
        let host_alias = target.label();

        let snapshot = read_remote_snapshot(&app_type, &target)?;
        let read = remote_settings_from_snapshot(&app_type, &snapshot)?;
        let settings_config = read.settings_config.ok_or_else(|| {
            AppError::Message(format!(
                "远端 {host_alias} 没有可导入的 {} 配置",
                app_type.as_str()
            ))
        })?;

        if let Some(existing_id) =
            find_matching_local_provider(state, &app_type, &snapshot, Some(&settings_config))?
        {
            let providers = state.db.get_all_providers(app_type.as_str())?;
            let provider = providers
                .get(&existing_id)
                .cloned()
                .ok_or_else(|| AppError::Message(format!("本地供应商 {existing_id} 不存在")))?;
            return Ok(RemoteImportResult {
                host_alias: host_alias.to_string(),
                app: app_type.as_str().to_string(),
                provider,
            });
        }

        let provider_id = generate_remote_provider_id(state, &app_type, host_alias)?;
        let mut provider = Provider::with_id(
            provider_id,
            format!("远端 {host_alias}"),
            settings_config,
            None,
        );
        provider.category = Some("custom".to_string());
        provider.created_at = Some(chrono::Utc::now().timestamp_millis());
        provider.notes = Some(format!(
            "Downloaded from SSH host {host_alias} for {}",
            app_type.as_str()
        ));

        state.db.save_provider(app_type.as_str(), &provider)?;

        Ok(RemoteImportResult {
            host_alias: host_alias.to_string(),
            app: app_type.as_str().to_string(),
            provider,
        })
    }

    /// Stops every process of the SSH user that belongs to the app (CLI and
    /// IDE-extension binaries), so the next launch picks up the new config.
    pub fn restart_remote_app_processes(
        app_type: AppType,
        target: &SshConnectionTarget,
    ) -> Result<RemoteRestartResult, AppError> {
        ensure_remote_supported(&app_type)?;
        let target = resolve_ssh_target(target)?;

        let command = format!("sh -s -- {}", app_type.as_str());
        let output = run_ssh_command(
            &target,
            &command,
            Some(REMOTE_STOP_APP_PROCESSES_SCRIPT.as_bytes()),
        )?;
        let (stopped, force_killed) = parse_stop_processes_output(&output);

        Ok(RemoteRestartResult {
            host_alias: target.label().to_string(),
            app: app_type.as_str().to_string(),
            stopped,
            force_killed,
        })
    }
}

/// Run as `sh -s -- <app>`. Matches on the executable name (plus node/bun
/// wrappers of the npm packages) so the pipeline's own awk/sh never match, and
/// skips IDE server/extension-host processes so the editor survives.
const REMOTE_STOP_APP_PROCESSES_SCRIPT: &str = r#"app="$1"
self=$$
targets=$(ps -u "$(id -u)" -o pid= -o comm= -o args= 2>/dev/null | awk -v self="$self" -v app="$app" '
{
  pid = $1
  n = split($2, parts, "/")
  comm = parts[n]
  if (pid == self) next
  args = $0
  sub(/^[ \t]*[0-9]+[ \t]+[^ \t]+[ \t]*/, "", args)
  if (args ~ /(extensionHost|bootstrap-fork|server-main\.js)/) next
  wrapper = (comm ~ /^(node|bun)$/)
  hit = 0
  if (app == "codex") {
    hit = (comm ~ /^codex/) || (wrapper && args ~ /(@openai\/codex|\/codex( |$))/)
  } else if (app == "claude") {
    hit = (comm == "claude") || (wrapper && args ~ /(@anthropic-ai\/claude-code|claude-code\/cli|\/claude( |$))/)
  } else if (app == "gemini") {
    hit = (comm == "gemini") || (wrapper && args ~ /(gemini-cli|\/gemini( |$))/)
  }
  if (hit) printf "%s\t%s\n", pid, args
}')
[ -z "$targets" ] && exit 0
printf '%s\n' "$targets"
pids=$(printf '%s\n' "$targets" | cut -f1)
kill -TERM $pids 2>/dev/null || true
alive=""
i=0
while [ "$i" -lt 5 ]; do
  sleep 1
  alive=""
  for p in $pids; do
    kill -0 "$p" 2>/dev/null && alive="$alive $p"
  done
  [ -z "$alive" ] && break
  i=$((i + 1))
done
if [ -n "$alive" ]; then
  kill -KILL $alive 2>/dev/null || true
  printf '__CC_SWITCH_FORCE_KILLED__%s\n' "$alive"
fi
"#;

fn parse_stop_processes_output(output: &str) -> (Vec<RemoteProcessInfo>, Vec<u32>) {
    const FORCE_MARKER: &str = "__CC_SWITCH_FORCE_KILLED__";
    let mut stopped = Vec::new();
    let mut force_killed = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(FORCE_MARKER) {
            force_killed.extend(
                rest.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            );
            continue;
        }
        let Some((pid, command)) = line.split_once('\t') else {
            continue;
        };
        if let Ok(pid) = pid.trim().parse() {
            stopped.push(RemoteProcessInfo {
                pid,
                command: command.trim().to_string(),
            });
        }
    }

    (stopped, force_killed)
}

pub(super) fn remote_state_from_snapshot(
    state: &AppState,
    app_type: &AppType,
    host_alias: &str,
    snapshot: &RemoteSnapshot,
) -> Result<RemoteProviderState, AppError> {
    let read = remote_settings_from_snapshot(app_type, snapshot)?;
    let has_existing_config = snapshot.has_any();
    let matched_provider_id =
        find_matching_local_provider(state, app_type, snapshot, read.settings_config.as_ref())?;
    let provider = read.settings_config.map(|settings_config| {
        let mut provider = Provider::with_id(
            "remote-current".to_string(),
            format!("远端当前配置 ({host_alias})"),
            settings_config,
            None,
        );
        provider.category = Some("custom".to_string());
        provider.notes = Some(format!(
            "Imported preview from SSH host {host_alias} for {}",
            app_type.as_str()
        ));
        provider
    });
    let via_gateway = super::remote_gateway::snapshot_uses_gateway(&state.db, host_alias, snapshot);
    let has_unmanaged_config = has_existing_config && matched_provider_id.is_none() && !via_gateway;

    Ok(RemoteProviderState {
        host_alias: host_alias.to_string(),
        app: app_type.as_str().to_string(),
        provider,
        matched_provider_id,
        files: snapshot.file_statuses(),
        has_existing_config,
        has_unmanaged_config,
        overwrite_warning: has_unmanaged_config
            .then(|| remote_overwrite_warning(app_type, host_alias)),
        warnings: read.warnings,
        via_gateway,
    })
}

/// Rejects a second switch to the same host/app while one is still running.
pub(super) struct InFlightGuard(String);

impl InFlightGuard {
    fn in_flight() -> &'static Mutex<HashSet<String>> {
        static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
    }

    pub(super) fn acquire(key: String) -> Option<Self> {
        let mut keys = Self::in_flight().lock().unwrap_or_else(|e| e.into_inner());
        keys.insert(key.clone()).then(|| Self(key))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut keys = Self::in_flight().lock().unwrap_or_else(|e| e.into_inner());
        keys.remove(&self.0);
    }
}

fn remote_overwrite_warning(app_type: &AppType, host_alias: &str) -> String {
    format!(
        "远端 {host_alias} 已有未同步的 {} 配置。切换会替换其中的供应商配置（MCP、项目信任等远端设置会保留），如需保留当前配置请先同步到本地。",
        app_type.as_str()
    )
}

fn remote_overwrite_block_message(app_type: &AppType, host_alias: &str) -> String {
    format!(
        "{} 如确认要覆盖，请重新点击确认。",
        remote_overwrite_warning(app_type, host_alias)
    )
}

pub(super) fn ensure_remote_supported(app_type: &AppType) -> Result<(), AppError> {
    if matches!(app_type, AppType::Claude | AppType::Codex | AppType::Gemini) {
        return Ok(());
    }

    Err(AppError::Message(format!(
        "远端配置暂时只支持 Claude、Codex 和 Gemini，当前应用为 {}",
        app_type.as_str()
    )))
}

pub(super) fn resolve_ssh_target(
    target: &SshConnectionTarget,
) -> Result<ResolvedSshTarget, AppError> {
    let is_manual = target
        .target_type
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("manual"))
        .unwrap_or(false)
        || target
            .host
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());

    if is_manual {
        return resolve_manual_ssh_target(target);
    }

    let alias = target
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Message("请选择 SSH Host".to_string()))?;
    ensure_known_ssh_host(alias)?;
    Ok(ResolvedSshTarget {
        label: alias.to_string(),
        connect_target: alias.to_string(),
        port: None,
        password: None,
    })
}

fn resolve_manual_ssh_target(target: &SshConnectionTarget) -> Result<ResolvedSshTarget, AppError> {
    let host = target
        .host
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Message("请输入 SSH 服务器 IP 或域名".to_string()))?;
    if !is_safe_manual_ssh_host(host) {
        return Err(AppError::Message(
            "SSH 服务器地址包含不受支持的字符".to_string(),
        ));
    }

    let user = target
        .user
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(user) = user {
        if !is_safe_ssh_user(user) {
            return Err(AppError::Message(
                "SSH 用户名包含不受支持的字符".to_string(),
            ));
        }
    }

    if matches!(target.port, Some(0)) {
        return Err(AppError::Message("SSH 端口必须在 1-65535 之间".to_string()));
    }

    let connect_target = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    };
    if connect_target.starts_with('-') {
        return Err(AppError::Message("SSH 连接目标不合法".to_string()));
    }

    let mut label = connect_target.clone();
    if let Some(port) = target.port {
        label = format!("{label}:{port}");
    }

    Ok(ResolvedSshTarget {
        label,
        connect_target,
        port: target.port,
        password: target
            .password
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned(),
    })
}

fn ensure_known_ssh_host(host_alias: &str) -> Result<(), AppError> {
    if !is_safe_ssh_alias(host_alias) {
        return Err(AppError::Message(format!(
            "SSH Host '{host_alias}' 包含不受支持的字符"
        )));
    }

    let hosts = RemoteProviderService::list_ssh_hosts()?;
    if hosts.iter().any(|host| host.alias == host_alias) {
        return Ok(());
    }

    Err(AppError::Message(format!(
        "SSH Host '{host_alias}' 不在 ~/.ssh/config 中"
    )))
}

/// A local provider matches when it was imported verbatim from the remote, or
/// when pushing it now would leave the remote files unchanged.
fn find_matching_local_provider(
    state: &AppState,
    app_type: &AppType,
    snapshot: &RemoteSnapshot,
    remote_settings: Option<&Value>,
) -> Result<Option<String>, AppError> {
    let providers = state.db.get_all_providers(app_type.as_str())?;

    if let Some(remote_settings) = remote_settings {
        if let Some(id) = providers
            .iter()
            .find_map(|(id, provider)| (provider.settings_config == *remote_settings).then_some(id))
        {
            return Ok(Some(id.clone()));
        }
    }

    if !snapshot.has_any() {
        return Ok(None);
    }

    Ok(providers.iter().find_map(|(id, provider)| {
        let writes = build_remote_writes(state, app_type, provider, snapshot).ok()?;
        remote_writes_match_snapshot(&writes, snapshot).then(|| id.clone())
    }))
}

fn remote_writes_match_snapshot(writes: &[RemoteWrite], snapshot: &RemoteSnapshot) -> bool {
    writes.iter().all(
        |write| match (write.content.as_deref(), snapshot.get(write.path)) {
            (None, None) => true,
            (Some(expected), Some(actual)) => {
                remote_contents_equivalent(write.path, expected, actual)
            }
            _ => false,
        },
    )
}

fn remote_contents_equivalent(path: &str, expected: &str, actual: &str) -> bool {
    if path.ends_with(".json") {
        if let (Ok(expected), Ok(actual)) = (
            serde_json::from_str::<Value>(expected),
            serde_json::from_str::<Value>(actual),
        ) {
            return expected == actual;
        }
    } else if path.ends_with(".toml") {
        if let (Some(expected), Some(actual)) = (
            codex_config_identity(expected),
            codex_config_identity(actual),
        ) {
            return expected == actual;
        }
    } else if path.ends_with(".env") {
        return crate::gemini_config::parse_env_file(expected)
            == crate::gemini_config::parse_env_file(actual);
    }
    expected.trim() == actual.trim()
}

/// Codex itself records trusted projects and dismissed notices in config.toml,
/// so those tables must not make an otherwise identical config look foreign.
fn codex_config_identity(config_text: &str) -> Option<toml::Table> {
    let mut table = toml::from_str::<toml::Table>(config_text).ok()?;
    table.remove("projects");
    table.remove("notice");
    Some(table)
}

fn generate_remote_provider_id(
    state: &AppState,
    app_type: &AppType,
    host_alias: &str,
) -> Result<String, AppError> {
    let existing_ids = state.db.get_provider_ids(app_type.as_str())?;
    let base = format!(
        "remote-{}-{}",
        app_type.as_str(),
        slugify_id_fragment(host_alias)
    );
    if !existing_ids.contains(&base) {
        return Ok(base);
    }

    for suffix in 2..1000 {
        let candidate = format!("{base}-{suffix}");
        if !existing_ids.contains(&candidate) {
            return Ok(candidate);
        }
    }

    Err(AppError::Message(format!(
        "无法为远端 {host_alias} 生成唯一供应商 ID"
    )))
}

fn slugify_id_fragment(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_' | '.' | '@' | ':') {
            Some('-')
        } else {
            None
        };

        let Some(next) = next else {
            continue;
        };
        if next == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
            slug.push(next);
        } else {
            last_dash = false;
            slug.push(next);
        }
    }

    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "host".to_string()
    } else {
        trimmed.to_string()
    }
}

fn build_remote_writes(
    state: &AppState,
    app_type: &AppType,
    provider: &Provider,
    snapshot: &RemoteSnapshot,
) -> Result<Vec<RemoteWrite>, AppError> {
    let effective_settings =
        build_effective_settings_with_common_config(state.db.as_ref(), app_type, provider)?;

    match app_type {
        AppType::Claude => {
            let settings = merge_remote_claude_settings(
                &sanitize_claude_settings_for_live(&effective_settings),
                snapshot.get(CLAUDE_SETTINGS_PATH),
            )?;
            Ok(vec![RemoteWrite {
                path: CLAUDE_SETTINGS_PATH,
                content: Some(json_pretty(&settings)?),
            }])
        }
        AppType::Codex => build_remote_codex_writes(provider, &effective_settings, snapshot),
        AppType::Gemini => build_remote_gemini_writes(provider, &effective_settings, snapshot),
        _ => unreachable!("unsupported app type checked by caller"),
    }
}

/// Mirrors the local Codex switch plan (bearer-token injection, auth.json
/// ownership, safety gates) against the remote files.
pub(super) fn build_remote_codex_writes(
    provider: &Provider,
    effective_settings: &Value,
    snapshot: &RemoteSnapshot,
) -> Result<Vec<RemoteWrite>, AppError> {
    let obj = effective_settings
        .as_object()
        .ok_or_else(|| AppError::Config("Codex 供应商配置必须是 JSON 对象".to_string()))?;
    let auth = obj
        .get("auth")
        .ok_or_else(|| AppError::Config("Codex 供应商配置缺少 'auth' 字段".to_string()))?;
    let config_text = obj.get("config").and_then(Value::as_str);

    let category = if crate::proxy::providers::is_codex_official_provider(provider) {
        Some("official")
    } else {
        provider.category.as_deref()
    };
    let plan = crate::codex_config::plan_codex_live_write(
        category,
        auth,
        config_text,
        crate::settings::preserve_codex_official_auth_on_switch(),
    )?;

    let mut writes = Vec::new();
    if plan.write_full_auth {
        writes.push(RemoteWrite {
            path: CODEX_AUTH_PATH,
            content: Some(json_pretty(auth)?),
        });
    } else if plan.remove_auth_file {
        writes.push(RemoteWrite {
            path: CODEX_AUTH_PATH,
            content: None,
        });
    }

    if let Some(config_text) = plan.config_text {
        let anchor = snapshot
            .get(CODEX_CONFIG_PATH)
            .filter(|text| !text.trim().is_empty());
        let config_text = anchor_codex_model_provider_id(&config_text, anchor)?;
        writes.push(RemoteWrite {
            path: CODEX_CONFIG_PATH,
            content: Some(preserve_remote_codex_tables(&config_text, anchor)?),
        });
    }

    Ok(writes)
}

/// Tables that belong to the remote host rather than to the provider: its MCP
/// servers (provider snapshots are stored without them) and the project trust
/// and notice state Codex records itself.
const REMOTE_OWNED_CODEX_TABLES: &[&str] = &["mcp_servers", "projects", "notice"];

fn preserve_remote_codex_tables(
    config_text: &str,
    remote_config_text: Option<&str>,
) -> Result<String, AppError> {
    let Some(remote_doc) = remote_config_text.and_then(|text| text.parse::<DocumentMut>().ok())
    else {
        return Ok(config_text.to_string());
    };
    if !REMOTE_OWNED_CODEX_TABLES
        .iter()
        .any(|key| remote_doc.contains_key(key))
    {
        return Ok(config_text.to_string());
    }

    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;
    for key in REMOTE_OWNED_CODEX_TABLES {
        if let Some(item) = remote_doc.get(key) {
            doc.insert(key, item.clone());
        }
    }
    Ok(doc.to_string())
}

/// Replaces only the provider-owned part of the remote settings.json (endpoint,
/// credentials, model mapping) and keeps the host's other settings. Local
/// common-config keys fill in only what the remote doesn't set itself.
pub(super) fn merge_remote_claude_settings(
    provider_settings: &Value,
    remote_settings_text: Option<&str>,
) -> Result<Value, AppError> {
    let Some(remote) = remote_settings_text
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
    else {
        return Ok(provider_settings.clone());
    };

    let common_of = |settings: &Value| -> Result<Map<String, Value>, AppError> {
        let text = super::ProviderService::extract_claude_common_config(settings)?;
        Ok(serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default())
    };
    let local_common = common_of(provider_settings)?;
    let remote_common = common_of(&remote)?;

    let mut merged = local_common.clone();
    overlay_claude_settings(&mut merged, &remote_common, |_, _| true);
    if let Some(provider_obj) = provider_settings.as_object() {
        let local_common_env = local_common.get("env").and_then(Value::as_object);
        overlay_claude_settings(&mut merged, provider_obj, |key, env_key| match env_key {
            Some(env_key) => !local_common_env.is_some_and(|env| env.contains_key(env_key)),
            None => !local_common.contains_key(key),
        });
    }

    Ok(Value::Object(merged))
}

/// Copies `source` into `target`, merging `env` per variable. `include` gets the
/// top-level key and, inside `env`, the variable name.
fn overlay_claude_settings(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    include: impl Fn(&str, Option<&str>) -> bool,
) {
    for (key, value) in source {
        if key == "env" {
            let Some(source_env) = value.as_object() else {
                continue;
            };
            let entries: Vec<_> = source_env
                .iter()
                .filter(|(env_key, _)| include(key, Some(env_key)))
                .collect();
            if entries.is_empty() {
                continue;
            }
            let target_env = target
                .entry("env")
                .or_insert_with(|| Value::Object(Map::new()));
            if !target_env.is_object() {
                *target_env = Value::Object(Map::new());
            }
            if let Some(target_env) = target_env.as_object_mut() {
                for (env_key, env_value) in entries {
                    target_env.insert(env_key.clone(), env_value.clone());
                }
            }
        } else if include(key, None) {
            target.insert(key.clone(), value.clone());
        }
    }
}

pub(super) fn active_codex_model_provider_id(doc: &DocumentMut) -> Option<String> {
    doc.get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Keeps the remote's existing custom `model_provider` id when switching, so
/// Codex session history on that host stays under a single provider bucket.
fn anchor_codex_model_provider_id(
    config_text: &str,
    anchor_config_text: Option<&str>,
) -> Result<String, AppError> {
    if config_text.trim().is_empty() {
        return Ok(config_text.to_string());
    }

    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    let Some(source_id) = active_codex_model_provider_id(&doc) else {
        return Ok(config_text.to_string());
    };
    if !crate::codex_config::is_custom_codex_model_provider_id(&source_id) {
        return Ok(config_text.to_string());
    }

    let Some(anchor_id) = anchor_config_text
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .and_then(|anchor_doc| active_codex_model_provider_id(&anchor_doc))
        .filter(|id| crate::codex_config::is_custom_codex_model_provider_id(id))
    else {
        return Ok(config_text.to_string());
    };
    if anchor_id == source_id {
        return Ok(config_text.to_string());
    }

    let Some(model_providers) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(config_text.to_string());
    };
    if model_providers.contains_key(&anchor_id) {
        return Ok(config_text.to_string());
    }
    let Some(provider_table) = model_providers.remove(&source_id) else {
        return Ok(config_text.to_string());
    };
    model_providers.insert(&anchor_id, provider_table);

    if let Some(profiles) = doc
        .get_mut("profiles")
        .and_then(|item| item.as_table_like_mut())
    {
        let profile_keys: Vec<String> = profiles.iter().map(|(key, _)| key.to_string()).collect();
        for profile_key in profile_keys {
            let Some(profile) = profiles
                .get_mut(&profile_key)
                .and_then(|item| item.as_table_like_mut())
            else {
                continue;
            };
            if profile.get("model_provider").and_then(|item| item.as_str())
                == Some(source_id.as_str())
            {
                profile.insert("model_provider", toml_edit::value(anchor_id.as_str()));
            }
        }
    }
    doc["model_provider"] = toml_edit::value(anchor_id.as_str());

    Ok(doc.to_string())
}

fn build_remote_gemini_writes(
    provider: &Provider,
    effective_settings: &Value,
    snapshot: &RemoteSnapshot,
) -> Result<Vec<RemoteWrite>, AppError> {
    use crate::gemini_config::{json_to_env, serialize_env_file, validate_gemini_settings_strict};

    let auth_type = detect_gemini_auth_type(provider);
    if matches!(
        auth_type,
        GeminiAuthType::Packycode | GeminiAuthType::Generic
    ) {
        validate_gemini_settings_strict(effective_settings)?;
    }

    let env_map = json_to_env(effective_settings)?;
    let env_text = serialize_env_file(&env_map);

    let mut settings_json =
        parse_json_object_or_empty(snapshot.get(GEMINI_SETTINGS_PATH).unwrap_or_default());

    if let Some(config_value) = effective_settings.get("config") {
        if let Some(config_obj) = config_value.as_object() {
            if let Some(target_obj) = settings_json.as_object_mut() {
                for (key, value) in config_obj {
                    target_obj.insert(key.clone(), value.clone());
                }
            }
        } else if !config_value.is_null() {
            return Err(AppError::localized(
                "gemini.validation.invalid_config",
                "Gemini 配置格式错误: config 必须是对象或 null",
                "Gemini config invalid: config must be an object or null",
            ));
        }
    }

    let selected_type = match auth_type {
        GeminiAuthType::GoogleOfficial => "oauth-personal",
        GeminiAuthType::Packycode | GeminiAuthType::Generic => "gemini-api-key",
    };
    set_gemini_selected_type(&mut settings_json, selected_type);

    Ok(vec![
        RemoteWrite {
            path: GEMINI_ENV_PATH,
            content: Some(env_text),
        },
        RemoteWrite {
            path: GEMINI_SETTINGS_PATH,
            content: Some(json_pretty(&settings_json)?),
        },
    ])
}

pub(super) fn remote_config_paths(app_type: &AppType) -> &'static [&'static str] {
    match app_type {
        AppType::Claude => &[CLAUDE_SETTINGS_PATH],
        AppType::Codex => &[CODEX_AUTH_PATH, CODEX_CONFIG_PATH],
        AppType::Gemini => &[GEMINI_ENV_PATH, GEMINI_SETTINGS_PATH],
        _ => &[],
    }
}

pub(super) fn read_remote_snapshot(
    app_type: &AppType,
    target: &ResolvedSshTarget,
) -> Result<RemoteSnapshot, AppError> {
    let paths = remote_config_paths(app_type);
    let script = build_read_files_script(paths);
    let output = run_ssh_command_bytes(target, "sh -s", Some(script.as_bytes()))?;
    let contents = parse_read_files_output(&output, paths.len())?;
    Ok(RemoteSnapshot {
        files: paths.iter().copied().zip(contents).collect(),
    })
}

const REMOTE_FILE_HEADER: &str = "__CC_SWITCH_FILE__ ";

/// Prints `<header> <bytes>\n<content>` per file (`-` when missing), so all
/// files come back in one SSH round trip and are split by exact byte length.
fn build_read_files_script(paths: &[&str]) -> String {
    let mut script = String::new();
    for path in paths {
        script.push_str(&format!(
            "f=\"{path}\"; if [ -f \"$f\" ]; then printf '{REMOTE_FILE_HEADER}%s\\n' \"$(wc -c < \"$f\" | tr -d ' ')\"; cat \"$f\"; else printf '{REMOTE_FILE_HEADER}-\\n'; fi\n"
        ));
    }
    script
}

fn parse_read_files_output(output: &[u8], count: usize) -> Result<Vec<Option<String>>, AppError> {
    let invalid = || AppError::Message("读取远端配置失败: 返回内容格式异常".to_string());
    let header = REMOTE_FILE_HEADER.as_bytes();
    let mut rest = output;
    let mut files = Vec::with_capacity(count);

    for _ in 0..count {
        let start = rest
            .windows(header.len())
            .position(|window| window == header)
            .ok_or_else(invalid)?;
        rest = &rest[start + header.len()..];
        let line_end = rest.iter().position(|b| *b == b'\n').ok_or_else(invalid)?;
        let size = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| invalid())?
            .trim();
        rest = &rest[line_end + 1..];

        if size == "-" {
            files.push(None);
            continue;
        }
        let size: usize = size.parse().map_err(|_| invalid())?;
        if rest.len() < size {
            return Err(invalid());
        }
        files.push(Some(String::from_utf8_lossy(&rest[..size]).into_owned()));
        rest = &rest[size..];
    }

    Ok(files)
}

/// Applies every write in one SSH round trip; each file is replaced atomically.
pub(super) fn apply_remote_writes(
    target: &ResolvedSshTarget,
    writes: &[RemoteWrite],
) -> Result<(Vec<String>, Vec<String>), AppError> {
    if writes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let script = build_write_files_script(writes)?;
    let output = run_ssh_command(target, "sh -s", Some(script.as_bytes()))?;

    let mut written = Vec::new();
    let mut removed = Vec::new();
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("W ") {
            written.push(path.to_string());
        } else if let Some(path) = line.strip_prefix("R ") {
            removed.push(path.to_string());
        }
    }
    Ok((written, removed))
}

fn build_write_files_script(writes: &[RemoteWrite]) -> Result<String, AppError> {
    let mut script = String::from(
        "set -e\numask 077\nwrite_file() { file=\"$1\"; mkdir -p \"${file%/*}\"; tmp=\"$file.tmp.$$\"; printf '%s' \"$2\" > \"$tmp\"; mv \"$tmp\" \"$file\"; chmod 600 \"$file\" 2>/dev/null || true; printf 'W %s\\n' \"$file\"; }\n",
    );
    for write in writes {
        match &write.content {
            Some(content) => {
                if content.contains('\0') {
                    return Err(AppError::Message(format!(
                        "{} 含有 NUL 字符，无法写入远端",
                        write.path
                    )));
                }
                script.push_str(&format!(
                    "write_file \"{}\" {}\n",
                    write.path,
                    shell_single_quote(content)
                ));
            }
            None => script.push_str(&format!(
                "rm -f \"{0}\"; printf 'R %s\\n' \"{0}\"\n",
                write.path
            )),
        }
    }
    Ok(script)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_settings_from_snapshot(
    app_type: &AppType,
    snapshot: &RemoteSnapshot,
) -> Result<RemoteSettingsRead, AppError> {
    match app_type {
        AppType::Claude => {
            let Some(content) = snapshot.get(CLAUDE_SETTINGS_PATH) else {
                return Ok(RemoteSettingsRead {
                    settings_config: None,
                    warnings: vec!["远端未找到 Claude Code settings.json".to_string()],
                });
            };

            Ok(RemoteSettingsRead {
                settings_config: Some(parse_remote_json(CLAUDE_SETTINGS_PATH, content)?),
                warnings: Vec::new(),
            })
        }
        AppType::Codex => {
            let auth_content = snapshot.get(CODEX_AUTH_PATH);
            let config_content = snapshot.get(CODEX_CONFIG_PATH);

            let mut warnings = Vec::new();
            if auth_content.is_none() {
                warnings.push("远端未找到 Codex auth.json".to_string());
            }
            if config_content.is_none() {
                warnings.push("远端未找到 Codex config.toml".to_string());
            }
            if auth_content.is_none() && config_content.is_none() {
                return Ok(RemoteSettingsRead {
                    settings_config: None,
                    warnings,
                });
            }

            let auth = match auth_content {
                Some(content) => parse_remote_json(CODEX_AUTH_PATH, content)?,
                None => json!({}),
            };
            Ok(RemoteSettingsRead {
                settings_config: Some(
                    json!({ "auth": auth, "config": config_content.unwrap_or_default() }),
                ),
                warnings,
            })
        }
        AppType::Gemini => {
            let env_content = snapshot.get(GEMINI_ENV_PATH);
            let settings_content = snapshot.get(GEMINI_SETTINGS_PATH);

            if env_content.is_none() && settings_content.is_none() {
                return Ok(RemoteSettingsRead {
                    settings_config: None,
                    warnings: vec!["远端未找到 Gemini .env 或 settings.json".to_string()],
                });
            }

            let mut warnings = Vec::new();
            if env_content.is_none() {
                warnings.push("远端未找到 Gemini .env".to_string());
            }
            if settings_content.is_none() {
                warnings.push("远端未找到 Gemini settings.json".to_string());
            }

            let env_map = env_content
                .map(crate::gemini_config::parse_env_file)
                .unwrap_or_default();
            let env_json = crate::gemini_config::env_to_json(&env_map);
            let env_obj = env_json.get("env").cloned().unwrap_or_else(|| json!({}));
            let settings = match settings_content {
                Some(content) => {
                    let value = parse_remote_json(GEMINI_SETTINGS_PATH, content)?;
                    if !value.is_object() {
                        return Err(AppError::Message(
                            "远端 Gemini settings.json 必须是 JSON 对象".to_string(),
                        ));
                    }
                    value
                }
                None => json!({}),
            };

            Ok(RemoteSettingsRead {
                settings_config: Some(json!({ "env": env_obj, "config": settings })),
                warnings,
            })
        }
        _ => unreachable!("unsupported app type checked by caller"),
    }
}

fn parse_remote_json(path: &str, content: &str) -> Result<Value, AppError> {
    serde_json::from_str::<Value>(content)
        .map_err(|e| AppError::Message(format!("解析远端 {path} 失败: {e}")))
}

pub(super) fn parse_json_object_or_empty(content: &str) -> Value {
    serde_json::from_str::<Value>(content)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

pub(super) fn set_gemini_selected_type(settings: &mut Value, selected_type: &str) {
    let Some(root) = settings.as_object_mut() else {
        *settings = json!({});
        return set_gemini_selected_type(settings, selected_type);
    };

    let security = root.entry("security").or_insert_with(|| json!({}));
    if !security.is_object() {
        *security = json!({});
    }

    let Some(security_obj) = security.as_object_mut() else {
        return;
    };
    let auth = security_obj.entry("auth").or_insert_with(|| json!({}));
    if !auth.is_object() {
        *auth = json!({});
    }

    if let Some(auth_obj) = auth.as_object_mut() {
        auth_obj.insert(
            "selectedType".to_string(),
            Value::String(selected_type.to_string()),
        );
    }
}

pub(super) fn json_pretty(value: &Value) -> Result<String, AppError> {
    serde_json::to_string_pretty(value).map_err(|e| AppError::JsonSerialize { source: e })
}

pub(super) fn run_ssh_command(
    target: &ResolvedSshTarget,
    remote_command: &str,
    stdin: Option<&[u8]>,
) -> Result<String, AppError> {
    let output = run_ssh_command_bytes(target, remote_command, stdin)?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn run_ssh_command_bytes(
    target: &ResolvedSshTarget,
    remote_command: &str,
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, AppError> {
    let mut command = Command::new("ssh");
    hide_ssh_console_window(&mut command);
    configure_ssh_command(&mut command, target, remote_command);
    let _askpass = configure_ssh_password(&mut command, target)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = command
        .spawn()
        .map_err(|e| AppError::Message(format!("启动 ssh 失败: {e}")))?;

    if let Some(input) = stdin {
        let Some(mut child_stdin) = child.stdin.take() else {
            return Err(AppError::Message("无法写入 ssh stdin".to_string()));
        };
        child_stdin
            .write_all(input)
            .map_err(|e| AppError::Message(format!("写入 ssh stdin 失败: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| AppError::Message(format!("等待 ssh 结束失败: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Message(if stderr.is_empty() {
            format!("ssh 命令失败，退出码: {}", output.status)
        } else {
            format!("ssh 命令失败: {stderr}")
        }));
    }

    Ok(output.stdout)
}

#[cfg(windows)]
fn hide_ssh_console_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_ssh_console_window(_command: &mut Command) {}

fn configure_ssh_command(command: &mut Command, target: &ResolvedSshTarget, remote_command: &str) {
    command.arg("-C");
    command.args(["-o", "ConnectTimeout=10", "-o", "ClearAllForwardings=yes"]);
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }

    if target.password.is_some() {
        command.args([
            "-o",
            "BatchMode=no",
            "-o",
            "NumberOfPasswordPrompts=1",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
    } else {
        command.args(["-o", "BatchMode=yes", "-o", "NumberOfPasswordPrompts=0"]);
    }
    configure_ssh_multiplexing(command);

    command
        .arg("--")
        .arg(&target.connect_target)
        .arg(remote_command);
}

pub(super) fn configure_ssh_password(
    command: &mut Command,
    target: &ResolvedSshTarget,
) -> Result<Option<tempfile::NamedTempFile>, AppError> {
    let Some(password) = target.password.as_deref() else {
        return Ok(None);
    };

    let mut file = tempfile::Builder::new()
        .prefix("cc-switch-ssh-askpass-")
        .suffix(askpass_script_suffix())
        .tempfile()
        .map_err(|e| AppError::Message(format!("创建 SSH 密码辅助脚本失败: {e}")))?;
    file.write_all(askpass_script_content().as_bytes())
        .map_err(|e| AppError::Message(format!("写入 SSH 密码辅助脚本失败: {e}")))?;

    #[cfg(unix)]
    {
        let mut permissions = file
            .as_file()
            .metadata()
            .map_err(|e| AppError::Message(format!("读取 SSH 密码辅助脚本权限失败: {e}")))?
            .permissions();
        permissions.set_mode(0o700);
        file.as_file()
            .set_permissions(permissions)
            .map_err(|e| AppError::Message(format!("设置 SSH 密码辅助脚本权限失败: {e}")))?;
    }

    command.env("SSH_ASKPASS", file.path());
    command.env("SSH_ASKPASS_REQUIRE", "force");
    command.env("CC_SWITCH_SSH_PASSWORD", password);
    if std::env::var_os("DISPLAY").is_none() {
        command.env("DISPLAY", "cc-switch");
    }

    Ok(Some(file))
}

#[cfg(unix)]
fn askpass_script_suffix() -> &'static str {
    ".sh"
}

#[cfg(not(unix))]
fn askpass_script_suffix() -> &'static str {
    ".cmd"
}

#[cfg(unix)]
fn askpass_script_content() -> &'static str {
    "#!/bin/sh\nprintf '%s\\n' \"$CC_SWITCH_SSH_PASSWORD\"\n"
}

#[cfg(not(unix))]
fn askpass_script_content() -> &'static str {
    "@echo off\r\necho %CC_SWITCH_SSH_PASSWORD%\r\n"
}

/// Reuses one SSH connection per host for a couple of minutes, so reading,
/// writing and re-inspecting don't each pay for a full handshake.
#[cfg(unix)]
fn configure_ssh_multiplexing(command: &mut Command) {
    let dir = crate::config::get_home_dir().join(".ssh");
    if !dir.is_dir() {
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    // ssh splits option values on whitespace; skip rather than risk a bad path.
    let control_path = dir.join("cc-switch-%C");
    let control_path = control_path.to_string_lossy();
    if control_path.chars().any(char::is_whitespace) {
        return;
    }
    command.args([
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={control_path}"),
        "-o".to_string(),
        "ControlPersist=120".to_string(),
    ]);
}

// Windows OpenSSH has no ControlMaster support.
#[cfg(not(unix))]
fn configure_ssh_multiplexing(_command: &mut Command) {}

fn is_safe_ssh_alias(alias: &str) -> bool {
    !alias.trim().is_empty()
        && !alias.starts_with('-')
        && alias
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@' | ':'))
}

fn is_safe_manual_ssh_host(host: &str) -> bool {
    !host.trim().is_empty()
        && !host.starts_with('-')
        && !host.contains('@')
        && host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '[' | ']'))
}

fn is_safe_ssh_user(user: &str) -> bool {
    !user.trim().is_empty()
        && !user.starts_with('-')
        && user
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn parse_ssh_config_file(
    path: &Path,
    hosts: &mut Vec<SshHostEntry>,
    index: &mut HashMap<String, usize>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    if !path.exists() {
        return Ok(());
    }

    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;
    parse_ssh_config_content(&content, path, hosts, index, visited)
}

fn parse_ssh_config_content(
    content: &str,
    source_path: &Path,
    hosts: &mut Vec<SshHostEntry>,
    index: &mut HashMap<String, usize>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    let mut current_aliases: Vec<String> = Vec::new();
    let source = source_path.to_string_lossy().to_string();
    let base_dir = source_path.parent().unwrap_or_else(|| Path::new("."));

    for raw_line in content.lines() {
        let line = raw_line
            .split_once('#')
            .map(|(before, _)| before)
            .unwrap_or(raw_line)
            .trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        let values: Vec<&str> = parts.collect();
        if values.is_empty() {
            continue;
        }

        match keyword.to_ascii_lowercase().as_str() {
            "include" => {
                for pattern in values {
                    for include_path in expand_include_pattern(pattern, base_dir)? {
                        parse_ssh_config_file(&include_path, hosts, index, visited)?;
                    }
                }
            }
            "host" => {
                current_aliases = values
                    .into_iter()
                    .filter_map(normalize_host_alias)
                    .collect::<Vec<_>>();
                for alias in &current_aliases {
                    if index.contains_key(alias) {
                        continue;
                    }
                    let position = hosts.len();
                    index.insert(alias.clone(), position);
                    hosts.push(SshHostEntry {
                        alias: alias.clone(),
                        host_name: None,
                        user: None,
                        port: None,
                        source: Some(source.clone()),
                    });
                }
            }
            "hostname" | "user" | "port" => {
                let value = values.join(" ");
                for alias in &current_aliases {
                    let Some(position) = index.get(alias).copied() else {
                        continue;
                    };
                    let host = &mut hosts[position];
                    match keyword.to_ascii_lowercase().as_str() {
                        "hostname" if host.host_name.is_none() => {
                            host.host_name = Some(value.clone())
                        }
                        "user" if host.user.is_none() => host.user = Some(value.clone()),
                        "port" if host.port.is_none() => host.port = Some(value.clone()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn normalize_host_alias(raw: &str) -> Option<String> {
    let alias = raw.trim().trim_matches('"').trim_matches('\'');
    if alias.is_empty()
        || alias.contains('*')
        || alias.contains('?')
        || alias.starts_with('!')
        || !is_safe_ssh_alias(alias)
    {
        return None;
    }
    Some(alias.to_string())
}

fn expand_include_pattern(pattern: &str, base_dir: &Path) -> Result<Vec<PathBuf>, AppError> {
    let path = expand_ssh_path(pattern, base_dir);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    if !file_name.contains('*') && !file_name.contains('?') {
        return Ok(vec![path]);
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };

    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if wildcard_match(&file_name, &name) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn expand_ssh_path(raw: &str, base_dir: &Path) -> PathBuf {
    let expanded = if raw == "~" {
        crate::config::get_home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        crate::config::get_home_dir().join(rest)
    } else {
        PathBuf::from(raw)
    };

    if expanded.is_relative() {
        base_dir.join(expanded)
    } else {
        expanded
    }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    fn inner(pattern: &[u8], value: &[u8]) -> bool {
        match (pattern.first(), value.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(b'*'), _) => {
                inner(&pattern[1..], value) || (!value.is_empty() && inner(pattern, &value[1..]))
            }
            (Some(b'?'), Some(_)) => inner(&pattern[1..], &value[1..]),
            (Some(a), Some(b)) if a == b => inner(&pattern[1..], &value[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn parse_ssh_config_collects_plain_hosts_and_metadata() {
        let content = r#"
Host *
  ServerAliveInterval 30

Host dev prod-box
  HostName 10.0.0.2
  User deploy
  Port 2200

Host ignored-*
  HostName ignored.example.com

Host "quoted"
  HostName quoted.example.com
"#;
        let mut hosts = Vec::new();
        let mut index = HashMap::new();
        let mut visited = HashSet::new();
        parse_ssh_config_content(
            content,
            Path::new("/tmp/ssh-config"),
            &mut hosts,
            &mut index,
            &mut visited,
        )
        .unwrap();

        assert_eq!(
            hosts
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["dev", "prod-box", "quoted"]
        );
        assert_eq!(hosts[0].host_name.as_deref(), Some("10.0.0.2"));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port.as_deref(), Some("2200"));
    }

    #[test]
    fn wildcard_match_supports_star_and_question_mark() {
        assert!(wildcard_match("conf.d/*", "conf.d/dev"));
        assert!(wildcard_match("host?.conf", "host1.conf"));
        assert!(!wildcard_match("host?.conf", "host12.conf"));
    }

    #[test]
    fn resolve_manual_ssh_target_builds_label_without_saving_password() -> Result<(), AppError> {
        let target = SshConnectionTarget {
            target_type: Some("manual".to_string()),
            alias: None,
            host: Some("10.0.0.2".to_string()),
            user: Some("deploy".to_string()),
            port: Some(2200),
            password: Some("secret".to_string()),
        };

        let resolved = resolve_ssh_target(&target)?;

        assert_eq!(resolved.label, "deploy@10.0.0.2:2200");
        assert_eq!(resolved.connect_target, "deploy@10.0.0.2");
        assert_eq!(resolved.port, Some(2200));
        assert_eq!(resolved.password.as_deref(), Some("secret"));
        Ok(())
    }

    #[test]
    fn resolve_manual_ssh_target_rejects_unsafe_host() {
        let target = SshConnectionTarget {
            target_type: Some("manual".to_string()),
            alias: None,
            host: Some("-oProxyCommand=bad".to_string()),
            user: Some("deploy".to_string()),
            port: Some(22),
            password: None,
        };

        assert!(resolve_ssh_target(&target).is_err());
    }

    #[test]
    fn anchor_codex_model_provider_id_keeps_remote_bucket() -> Result<(), AppError> {
        let config = r#"model_provider = "packy"
model = "gpt-5.5"

[model_providers.packy]
name = "packy"
base_url = "https://example.com/v1"
experimental_bearer_token = "sk-test"

[profiles.fast]
model_provider = "packy"
"#;
        let anchor =
            "model_provider = \"remote_main\"\n\n[model_providers.remote_main]\nname = \"old\"\n";

        let anchored = anchor_codex_model_provider_id(config, Some(anchor))?;
        let doc = anchored.parse::<DocumentMut>().unwrap();

        assert_eq!(doc["model_provider"].as_str(), Some("remote_main"));
        assert_eq!(
            doc["profiles"]["fast"]["model_provider"].as_str(),
            Some("remote_main")
        );
        assert_eq!(
            doc["model_providers"]["remote_main"]["experimental_bearer_token"].as_str(),
            Some("sk-test")
        );
        assert!(doc["model_providers"].get("packy").is_none());
        Ok(())
    }

    #[test]
    fn anchor_codex_model_provider_id_ignores_reserved_anchor() -> Result<(), AppError> {
        let config = "model_provider = \"packy\"\n\n[model_providers.packy]\nname = \"packy\"\n";

        let anchored =
            anchor_codex_model_provider_id(config, Some("model_provider = \"openai\"\n"))?;

        assert_eq!(anchored, config);
        Ok(())
    }

    #[test]
    fn remote_writes_match_ignores_codex_runtime_tables_and_json_formatting() {
        let snapshot = RemoteSnapshot {
            files: vec![
                (CODEX_AUTH_PATH, None),
                (
                    CODEX_CONFIG_PATH,
                    Some(
                        "model = \"gpt-5.5\"\n\n[projects.\"/srv/app\"]\ntrust_level = \"trusted\"\n"
                            .to_string(),
                    ),
                ),
                (CLAUDE_SETTINGS_PATH, Some("{\"env\":{\"A\":\"1\"}}".to_string())),
            ],
        };
        let writes = vec![
            RemoteWrite {
                path: CODEX_AUTH_PATH,
                content: None,
            },
            RemoteWrite {
                path: CODEX_CONFIG_PATH,
                content: Some("model = \"gpt-5.5\"\n".to_string()),
            },
            RemoteWrite {
                path: CLAUDE_SETTINGS_PATH,
                content: Some("{\n  \"env\": {\n    \"A\": \"1\"\n  }\n}".to_string()),
            },
        ];

        assert!(remote_writes_match_snapshot(&writes, &snapshot));

        let mismatched = [RemoteWrite {
            path: CODEX_CONFIG_PATH,
            content: Some("model = \"gpt-5.4\"\n".to_string()),
        }];
        assert!(!remote_writes_match_snapshot(&mismatched, &snapshot));
    }

    #[test]
    fn preserve_remote_codex_tables_keeps_remote_mcp_and_trust() -> Result<(), AppError> {
        let config = r#"model_provider = "packy"
model = "gpt-5.5"

[model_providers.packy]
base_url = "https://example.com/v1"

[mcp_servers.local_only]
command = "local"
"#;
        let remote = r#"model = "gpt-5.4"

[mcp_servers.remote_tool]
command = "remote"

[projects."/srv/app"]
trust_level = "trusted"
"#;

        let merged = preserve_remote_codex_tables(config, Some(remote))?;
        let doc = merged.parse::<DocumentMut>().unwrap();

        assert_eq!(doc["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(
            doc["model_providers"]["packy"]["base_url"].as_str(),
            Some("https://example.com/v1")
        );
        assert_eq!(
            doc["mcp_servers"]["remote_tool"]["command"].as_str(),
            Some("remote")
        );
        assert!(doc["mcp_servers"].get("local_only").is_none());
        assert_eq!(
            doc["projects"]["/srv/app"]["trust_level"].as_str(),
            Some("trusted")
        );

        assert_eq!(preserve_remote_codex_tables(config, None)?, config);
        Ok(())
    }

    #[test]
    fn merge_remote_claude_settings_replaces_only_provider_keys() -> Result<(), AppError> {
        let provider = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://new.example.com",
                "ANTHROPIC_AUTH_TOKEN": "new-token",
                "ANTHROPIC_MODEL": "new-model",
                "SHARED_FLAG": "local"
            },
            "includeCoAuthoredBy": false
        });
        let remote = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://old.example.com",
                "ANTHROPIC_API_KEY": "old-key",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "old-opus",
                "SHARED_FLAG": "remote",
                "HTTPS_PROXY": "http://proxy:3128"
            },
            "permissions": { "allow": ["Bash(ls)"] },
            "hooks": { "Stop": [] }
        })
        .to_string();

        let merged = merge_remote_claude_settings(&provider, Some(&remote))?;

        assert_eq!(
            merged,
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://new.example.com",
                    "ANTHROPIC_AUTH_TOKEN": "new-token",
                    "ANTHROPIC_MODEL": "new-model",
                    "SHARED_FLAG": "remote",
                    "HTTPS_PROXY": "http://proxy:3128"
                },
                "includeCoAuthoredBy": false,
                "permissions": { "allow": ["Bash(ls)"] },
                "hooks": { "Stop": [] }
            })
        );

        assert_eq!(merge_remote_claude_settings(&provider, None)?, provider);
        assert_eq!(
            merge_remote_claude_settings(&provider, Some("not json"))?,
            provider
        );
        Ok(())
    }

    #[cfg(unix)]
    fn run_local_sh(home: &Path, script: &str) -> Vec<u8> {
        let mut child = Command::new("sh")
            .arg("-s")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        output.stdout
    }

    #[cfg(unix)]
    #[test]
    fn batched_write_and_read_scripts_round_trip_through_sh() {
        let home = tempfile::tempdir().unwrap();
        let tricky = "model = \"it's $HOME `x` \\\\ \\n\"\n# 中文\nlast line without newline";
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(home.path().join(".codex/auth.json"), "{\"old\":true}").unwrap();

        let writes = [
            RemoteWrite {
                path: CODEX_CONFIG_PATH,
                content: Some(tricky.to_string()),
            },
            RemoteWrite {
                path: CODEX_AUTH_PATH,
                content: None,
            },
        ];
        let output = run_local_sh(home.path(), &build_write_files_script(&writes).unwrap());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("W ") && output.contains("R "));
        assert_eq!(
            fs::read_to_string(home.path().join(".codex/config.toml")).unwrap(),
            tricky
        );
        assert!(!home.path().join(".codex/auth.json").exists());

        let paths = [CODEX_AUTH_PATH, CODEX_CONFIG_PATH];
        let output = run_local_sh(home.path(), &build_read_files_script(&paths));
        assert_eq!(
            parse_read_files_output(&output, paths.len()).unwrap(),
            vec![None, Some(tricky.to_string())]
        );
    }

    #[test]
    fn parse_read_files_output_rejects_truncated_content() {
        let output = format!("{REMOTE_FILE_HEADER}10\nshort");
        assert!(parse_read_files_output(output.as_bytes(), 1).is_err());
        assert!(parse_read_files_output(b"", 1).is_err());
    }

    #[test]
    fn in_flight_guard_blocks_duplicate_until_dropped() {
        let first = InFlightGuard::acquire("test-host\ncodex".to_string());
        assert!(first.is_some());
        assert!(InFlightGuard::acquire("test-host\ncodex".to_string()).is_none());
        drop(first);
        assert!(InFlightGuard::acquire("test-host\ncodex".to_string()).is_some());
    }

    #[test]
    fn parse_stop_processes_output_collects_stopped_and_forced() {
        let output = "123\tcodex app-server\n456\tnode /usr/lib/node_modules/@openai/codex/bin/codex.js\n__CC_SWITCH_FORCE_KILLED__ 456\n";

        let (stopped, forced) = parse_stop_processes_output(output);

        assert_eq!(
            stopped,
            vec![
                RemoteProcessInfo {
                    pid: 123,
                    command: "codex app-server".to_string(),
                },
                RemoteProcessInfo {
                    pid: 456,
                    command: "node /usr/lib/node_modules/@openai/codex/bin/codex.js".to_string(),
                },
            ]
        );
        assert_eq!(forced, vec![456]);
        assert_eq!(parse_stop_processes_output(""), (Vec::new(), Vec::new()));
    }
}
