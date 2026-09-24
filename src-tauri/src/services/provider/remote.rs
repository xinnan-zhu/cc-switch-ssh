use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::DocumentMut;

use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::Provider;
use crate::store::AppState;

use super::gemini_auth::{detect_gemini_auth_type, GeminiAuthType};
use super::live::{build_effective_settings_with_common_config, sanitize_claude_settings_for_live};

const CLAUDE_SETTINGS_PATH: &str = "$HOME/.claude/settings.json";
const CODEX_AUTH_PATH: &str = "$HOME/.codex/auth.json";
const CODEX_CONFIG_PATH: &str = "$HOME/.codex/config.toml";
const GEMINI_ENV_PATH: &str = "$HOME/.gemini/.env";
const GEMINI_SETTINGS_PATH: &str = "$HOME/.gemini/settings.json";

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

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedSshTarget {
    label: String,
    connect_target: String,
    port: Option<u16>,
    password: Option<String>,
}

impl ResolvedSshTarget {
    fn label(&self) -> &str {
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImportResult {
    pub host_alias: String,
    pub app: String,
    pub provider: Provider,
}

/// Contents of the remote live config files for one app; `None` means missing.
struct RemoteSnapshot {
    files: Vec<(&'static str, Option<String>)>,
}

impl RemoteSnapshot {
    fn get(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(file_path, _)| *file_path == path)
            .and_then(|(_, content)| content.as_deref())
    }

    fn has_any(&self) -> bool {
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

/// `content: None` removes the remote file (after backing it up).
struct RemoteWrite {
    path: &'static str,
    backup_name: &'static str,
    content: Option<String>,
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

        let providers = state.db.get_all_providers(app_type.as_str())?;
        let provider = providers
            .get(provider_id)
            .ok_or_else(|| AppError::Message(format!("供应商 {provider_id} 不存在")))?;

        let snapshot = read_remote_snapshot(&app_type, &target)?;
        let has_existing_config = snapshot.has_any();
        if has_existing_config && !force_overwrite {
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

        let writes = build_remote_writes(state, &app_type, provider, &snapshot)?;
        let stamp = remote_backup_stamp();
        let mut written_files = Vec::new();
        let mut removed_files = Vec::new();

        for write in writes {
            match write.content {
                Some(content) => written_files.push(write_remote_file(
                    &target,
                    write.path,
                    write.backup_name,
                    content.as_bytes(),
                    &stamp,
                )?),
                None if snapshot.get(write.path).is_some() => removed_files.push(
                    remove_remote_file(&target, write.path, write.backup_name, &stamp)?,
                ),
                None => {}
            }
        }

        Ok(RemoteApplyResult {
            host_alias: host_alias.to_string(),
            app: app_type.as_str().to_string(),
            provider_id: provider_id.to_string(),
            written_files,
            removed_files,
            overwrote_existing_config: has_existing_config,
            warnings: Vec::new(),
        })
    }

    pub fn inspect_remote_provider(
        state: &AppState,
        app_type: AppType,
        target: &SshConnectionTarget,
    ) -> Result<RemoteProviderState, AppError> {
        ensure_remote_supported(&app_type)?;
        let target = resolve_ssh_target(target)?;
        let host_alias = target.label();

        let snapshot = read_remote_snapshot(&app_type, &target)?;
        let read = remote_settings_from_snapshot(&app_type, &snapshot)?;
        let has_existing_config = snapshot.has_any();
        let matched_provider_id = find_matching_local_provider(
            state,
            &app_type,
            &snapshot,
            read.settings_config.as_ref(),
        )?;
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
        let has_unmanaged_config = has_existing_config && matched_provider_id.is_none();

        Ok(RemoteProviderState {
            host_alias: host_alias.to_string(),
            app: app_type.as_str().to_string(),
            provider,
            matched_provider_id,
            files: snapshot.file_statuses(),
            has_existing_config,
            has_unmanaged_config,
            overwrite_warning: has_unmanaged_config
                .then(|| remote_overwrite_warning(&app_type, host_alias)),
            warnings: read.warnings,
        })
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
}

fn remote_overwrite_warning(app_type: &AppType, host_alias: &str) -> String {
    format!(
        "远端 {host_alias} 已有 {} 配置。切换会覆盖这些文件，建议先同步到本地；覆盖前会自动备份到远端 ~/.cc-switch/remote-backups。",
        app_type.as_str()
    )
}

fn remote_overwrite_block_message(app_type: &AppType, host_alias: &str) -> String {
    format!(
        "{} 如确认要覆盖，请重新点击确认。",
        remote_overwrite_warning(app_type, host_alias)
    )
}

fn ensure_remote_supported(app_type: &AppType) -> Result<(), AppError> {
    if matches!(app_type, AppType::Claude | AppType::Codex | AppType::Gemini) {
        return Ok(());
    }

    Err(AppError::Message(format!(
        "远端配置暂时只支持 Claude、Codex 和 Gemini，当前应用为 {}",
        app_type.as_str()
    )))
}

fn resolve_ssh_target(target: &SshConnectionTarget) -> Result<ResolvedSshTarget, AppError> {
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
            let settings = sanitize_claude_settings_for_live(&effective_settings);
            Ok(vec![RemoteWrite {
                path: CLAUDE_SETTINGS_PATH,
                backup_name: "claude-settings.json",
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
fn build_remote_codex_writes(
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
            backup_name: "codex-auth.json",
            content: Some(json_pretty(auth)?),
        });
    } else if plan.remove_auth_file {
        writes.push(RemoteWrite {
            path: CODEX_AUTH_PATH,
            backup_name: "codex-auth.json",
            content: None,
        });
    }

    if let Some(config_text) = plan.config_text {
        let anchor = snapshot
            .get(CODEX_CONFIG_PATH)
            .filter(|text| !text.trim().is_empty());
        writes.push(RemoteWrite {
            path: CODEX_CONFIG_PATH,
            backup_name: "codex-config.toml",
            content: Some(anchor_codex_model_provider_id(&config_text, anchor)?),
        });
    }

    Ok(writes)
}

fn active_codex_model_provider_id(doc: &DocumentMut) -> Option<String> {
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
            backup_name: "gemini-env",
            content: Some(env_text),
        },
        RemoteWrite {
            path: GEMINI_SETTINGS_PATH,
            backup_name: "gemini-settings.json",
            content: Some(json_pretty(&settings_json)?),
        },
    ])
}

fn remote_config_paths(app_type: &AppType) -> &'static [&'static str] {
    match app_type {
        AppType::Claude => &[CLAUDE_SETTINGS_PATH],
        AppType::Codex => &[CODEX_AUTH_PATH, CODEX_CONFIG_PATH],
        AppType::Gemini => &[GEMINI_ENV_PATH, GEMINI_SETTINGS_PATH],
        _ => &[],
    }
}

fn read_remote_snapshot(
    app_type: &AppType,
    target: &ResolvedSshTarget,
) -> Result<RemoteSnapshot, AppError> {
    let files = remote_config_paths(app_type)
        .iter()
        .map(|path| Ok((*path, read_remote_file(target, path)?)))
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(RemoteSnapshot { files })
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

fn parse_json_object_or_empty(content: &str) -> Value {
    serde_json::from_str::<Value>(content)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn set_gemini_selected_type(settings: &mut Value, selected_type: &str) {
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

fn json_pretty(value: &Value) -> Result<String, AppError> {
    serde_json::to_string_pretty(value).map_err(|e| AppError::JsonSerialize { source: e })
}

fn read_remote_file(
    target: &ResolvedSshTarget,
    remote_path: &'static str,
) -> Result<Option<String>, AppError> {
    const MARKER: &str = "__CC_SWITCH_REMOTE_FILE_EXISTS__\n";
    let command = format!(
        "file=\"{remote_path}\"; if [ -f \"$file\" ]; then printf '%s\\n' '__CC_SWITCH_REMOTE_FILE_EXISTS__'; cat \"$file\"; fi"
    );
    let output = run_ssh_command(target, &command, None)?;
    Ok(output.strip_prefix(MARKER).map(str::to_string))
}

fn write_remote_file(
    target: &ResolvedSshTarget,
    remote_path: &'static str,
    backup_name: &'static str,
    content: &[u8],
    stamp: &str,
) -> Result<String, AppError> {
    let command = format!(
        "set -e; umask 077; file=\"{remote_path}\"; dir=\"${{file%/*}}\"; backup_root=\"$HOME/.cc-switch/remote-backups/{stamp}\"; mkdir -p \"$dir\"; if [ -f \"$file\" ]; then mkdir -p \"$backup_root\"; cp \"$file\" \"$backup_root/{backup_name}\"; fi; tmp=\"$file.tmp.$$\"; cat > \"$tmp\"; mv \"$tmp\" \"$file\"; chmod 600 \"$file\" 2>/dev/null || true; printf '%s\\n' \"$file\""
    );
    let output = run_ssh_command(target, &command, Some(content))?;
    Ok(last_output_line(&output).unwrap_or(remote_path).to_string())
}

fn remove_remote_file(
    target: &ResolvedSshTarget,
    remote_path: &'static str,
    backup_name: &'static str,
    stamp: &str,
) -> Result<String, AppError> {
    let command = format!(
        "set -e; umask 077; file=\"{remote_path}\"; backup_root=\"$HOME/.cc-switch/remote-backups/{stamp}\"; if [ -f \"$file\" ]; then mkdir -p \"$backup_root\"; cp \"$file\" \"$backup_root/{backup_name}\"; rm -f \"$file\"; fi; printf '%s\\n' \"$file\""
    );
    let output = run_ssh_command(target, &command, None)?;
    Ok(last_output_line(&output).unwrap_or(remote_path).to_string())
}

fn last_output_line(output: &str) -> Option<&str> {
    output
        .lines()
        .last()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

fn run_ssh_command(
    target: &ResolvedSshTarget,
    remote_command: &str,
    stdin: Option<&[u8]>,
) -> Result<String, AppError> {
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

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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

    command
        .arg("--")
        .arg(&target.connect_target)
        .arg(remote_command);
}

fn configure_ssh_password(
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

fn remote_backup_stamp() -> String {
    chrono::Local::now().format("%Y%m%d%H%M%S").to_string()
}

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
                backup_name: "codex-auth.json",
                content: None,
            },
            RemoteWrite {
                path: CODEX_CONFIG_PATH,
                backup_name: "codex-config.toml",
                content: Some("model = \"gpt-5.5\"\n".to_string()),
            },
            RemoteWrite {
                path: CLAUDE_SETTINGS_PATH,
                backup_name: "claude-settings.json",
                content: Some("{\n  \"env\": {\n    \"A\": \"1\"\n  }\n}".to_string()),
            },
        ];

        assert!(remote_writes_match_snapshot(&writes, &snapshot));

        let mismatched = [RemoteWrite {
            path: CODEX_CONFIG_PATH,
            backup_name: "codex-config.toml",
            content: Some("model = \"gpt-5.4\"\n".to_string()),
        }];
        assert!(!remote_writes_match_snapshot(&mismatched, &snapshot));
    }
}
