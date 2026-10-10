//! cc-switch-ssh: versions of the CLIs installed on an SSH host, and updating
//! them in place.
//!
//! One SSH round trip probes every CLI (path, resolved target, `--version`).
//! The install source is read from the resolved target so the update goes
//! through the same channel: the npm next to the binary, the CLI's own
//! `update`, or Homebrew. Unknown sources are reported instead of guessed, so
//! an update never installs a second copy that the old one still shadows.

use serde::Serialize;

use super::remote::{
    resolve_ssh_target, run_ssh_command, shell_single_quote, InFlightGuard, ResolvedSshTarget,
};
use super::SshConnectionTarget;
use crate::app_config::AppType;
use crate::error::AppError;

/// `(app, executable)`; the order is the order shown in the UI.
const REMOTE_CLIS: &[(AppType, &str)] = &[
    (AppType::Claude, "claude"),
    (AppType::Codex, "codex"),
    (AppType::Gemini, "gemini"),
    (AppType::GrokBuild, "grok"),
];

const PROBE_MARKER: &str = "__CC_SWITCH_CLI__";
const EXIT_MARKER: &str = "__CC_SWITCH_EXIT__";
/// Output lines kept when an update fails.
const FAILURE_TAIL_LINES: usize = 12;

/// Same installer the local Grok lifecycle falls back to.
const GROK_INSTALL_UNIX: &str = "bash -c 'tmp=$(mktemp) && curl -fsSL https://x.ai/cli/install.sh -o $tmp && bash $tmp; status=$?; rm -f $tmp; exit $status'";

/// Non-interactive SSH sessions skip `.bashrc`/`.zshrc`, where nvm, fnm and
/// friends put node on PATH. Ask the login shell for its interactive PATH
/// (bounded by `timeout` when the host has it) and add the usual per-user
/// install directories as a fallback. `env` prints PATH colon-joined in every
/// shell, fish included.
const REMOTE_PATH_PRELUDE: &str = r#"limit() { if command -v timeout >/dev/null 2>&1; then timeout "$@"; else shift; "$@"; fi; }
login_path=$(limit 15 "${SHELL:-/bin/sh}" -ilc 'echo; env' </dev/null 2>/dev/null | sed -n 's/^PATH=//p' | tail -n 1)
[ -n "$login_path" ] && PATH="$login_path:$PATH"
PATH="$PATH:$HOME/.local/bin:$HOME/.npm-global/bin:$HOME/.grok/bin:$HOME/.volta/bin:$HOME/.bun/bin:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin"
export PATH
export NO_COLOR=1
"#;

/// Run as `sh -s -- <tool>...`; prints one tab-separated line per tool:
/// marker, tool, path, resolved target, first lines of `--version`.
const REMOTE_PROBE_SCRIPT: &str = r#"resolve() {
  r=$(realpath "$1" 2>/dev/null) || r=$(readlink -f "$1" 2>/dev/null) || r=""
  if [ -z "$r" ]; then
    l=$(readlink "$1" 2>/dev/null) || l=""
    case "$l" in
      "") r="$1" ;;
      /*) r="$l" ;;
      *) r="${1%/*}/$l" ;;
    esac
  fi
  printf '%s' "$r"
}
for tool in "$@"; do
  bin=$(command -v "$tool" 2>/dev/null) || bin=""
  case "$bin" in /*) ;; *) bin="" ;; esac
  if [ -z "$bin" ]; then
    printf '__CC_SWITCH_CLI__\t%s\t\t\t\n' "$tool"
    continue
  fi
  real=$(resolve "$bin")
  out=$(limit 20 "$bin" --version </dev/null 2>&1 | head -n 3 | tr '\t\n' '  ')
  printf '__CC_SWITCH_CLI__\t%s\t%s\t%s\t%s\n' "$tool" "$bin" "$real" "$out"
done
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteCliSource {
    /// Not found on the host's PATH.
    Missing,
    /// A global npm package; updated with the npm next to it.
    Npm,
    /// The vendor's own installer; updated with the CLI's `update`.
    Native,
    Brew,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCliInfo {
    pub app: String,
    pub tool: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub source: RemoteCliSource,
    /// Installed and the source is known, so it can be updated from here.
    pub updatable: bool,
    /// Found but `--version` printed no version.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCliReport {
    pub host_alias: String,
    pub clis: Vec<RemoteCliInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCliUpdateResult {
    pub host_alias: String,
    pub previous_version: Option<String>,
    pub cli: RemoteCliInfo,
    /// The command that ran on the host.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    tool: String,
    path: Option<String>,
    real: Option<String>,
    version_output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// `npm i -g` with the npm in the binary's directory, else the one on PATH.
    Npm {
        bin_dir: String,
    },
    SelfUpdate,
    Brew {
        cask: bool,
        name: String,
    },
}

pub struct RemoteCliService;

impl RemoteCliService {
    pub async fn versions(target: &SshConnectionTarget) -> Result<RemoteCliReport, AppError> {
        let target = resolve_ssh_target(target)?;
        let tools: Vec<&'static str> = REMOTE_CLIS.iter().map(|(_, tool)| *tool).collect();
        let probes = {
            let target = target.clone();
            tauri::async_runtime::spawn_blocking(move || probe(&target, &tools))
                .await
                .map_err(|e| AppError::Message(format!("检测远端 CLI 版本失败: {e}")))??
        };

        let clis = futures::future::join_all(probes.iter().map(|probe| async move {
            let mut info = cli_info(probe);
            if info.version.is_some() {
                info.latest_version = crate::commands::npm_latest_version_for_tool(
                    &probe.tool,
                    info.version.as_deref(),
                )
                .await;
            }
            info
        }))
        .await;

        Ok(RemoteCliReport {
            host_alias: target.label().to_string(),
            clis,
        })
    }

    pub async fn update(
        target: &SshConnectionTarget,
        app_type: AppType,
    ) -> Result<RemoteCliUpdateResult, AppError> {
        let tool = REMOTE_CLIS
            .iter()
            .find(|(app, _)| *app == app_type)
            .map(|(_, tool)| *tool)
            .ok_or_else(|| AppError::Message(format!("{} 不支持远端更新", app_type.as_str())))?;
        let target = resolve_ssh_target(target)?;
        let host_alias = target.label().to_string();

        let key = format!("{host_alias}\ncli\n{tool}");
        let (previous_version, after, command) = tauri::async_runtime::spawn_blocking(move || {
            let _in_flight = InFlightGuard::acquire(key).ok_or_else(|| {
                AppError::Message(format!("{} 上的 {tool} 正在更新，请稍候", target.label()))
            })?;
            let before = probe_one(&target, tool)?;
            let previous_version = cli_info(&before).version;
            let command = update_command(&before)?;
            run_update(&target, &command)?;
            let after = probe_one(&target, tool)?;
            Ok::<_, AppError>((previous_version, after, command))
        })
        .await
        .map_err(|e| AppError::Message(format!("更新远端 CLI 失败: {e}")))??;

        let mut cli = cli_info(&after);
        cli.latest_version =
            crate::commands::npm_latest_version_for_tool(tool, cli.version.as_deref()).await;
        Ok(RemoteCliUpdateResult {
            host_alias,
            previous_version,
            cli,
            command,
        })
    }
}

fn probe(target: &ResolvedSshTarget, tools: &[&str]) -> Result<Vec<Probe>, AppError> {
    let command = format!("sh -s -- {}", tools.join(" "));
    let script = format!("{REMOTE_PATH_PRELUDE}{REMOTE_PROBE_SCRIPT}");
    let output = run_ssh_command(target, &command, Some(script.as_bytes()))?;
    let probes = parse_probe_output(&output);
    tools
        .iter()
        .map(|tool| {
            probes
                .iter()
                .find(|probe| probe.tool == *tool)
                .cloned()
                .ok_or_else(|| AppError::Message(format!("检测远端 {tool} 失败: 返回内容格式异常")))
        })
        .collect()
}

fn probe_one(target: &ResolvedSshTarget, tool: &str) -> Result<Probe, AppError> {
    probe(target, &[tool])?
        .pop()
        .ok_or_else(|| AppError::Message(format!("检测远端 {tool} 失败")))
}

fn parse_probe_output(output: &str) -> Vec<Probe> {
    let non_empty = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix(PROBE_MARKER)?.split('\t').skip(1);
            let tool = fields.next()?.trim().to_string();
            let path = fields.next().and_then(non_empty);
            let real = fields.next().and_then(non_empty);
            let version_output = fields.collect::<Vec<_>>().join(" ").trim().to_string();
            Some(Probe {
                tool,
                path,
                real,
                version_output,
            })
        })
        .collect()
}

fn cli_info(probe: &Probe) -> RemoteCliInfo {
    let app = REMOTE_CLIS
        .iter()
        .find(|(_, tool)| *tool == probe.tool)
        .map(|(app, _)| app.as_str())
        .unwrap_or(probe.tool.as_str())
        .to_string();
    let version = extract_version(&probe.version_output);
    let source = install_source(probe);
    let error = (probe.path.is_some() && version.is_none()).then(|| {
        if probe.version_output.is_empty() {
            "--version 没有输出".to_string()
        } else {
            probe.version_output.chars().take(200).collect()
        }
    });
    RemoteCliInfo {
        app,
        tool: probe.tool.clone(),
        path: probe.path.clone(),
        version,
        latest_version: None,
        source,
        updatable: update_plan(probe).is_some(),
        error,
    }
}

fn extract_version(output: &str) -> Option<String> {
    static VERSION_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    VERSION_RE
        .get_or_init(|| regex::Regex::new(r"\d+\.\d+\.\d+(-[\w.]+)?").expect("valid regex"))
        .find(output)
        .map(|m| m.as_str().to_string())
}

fn install_source(probe: &Probe) -> RemoteCliSource {
    let (Some(path), Some(real)) = (probe.path.as_deref(), probe.real.as_deref()) else {
        return RemoteCliSource::Missing;
    };
    // npm first: globals of a Homebrew node also resolve under the brew prefix.
    if npm_package_dir(&probe.tool, real) {
        return RemoteCliSource::Npm;
    }
    if brew_package(real).is_some() {
        return RemoteCliSource::Brew;
    }
    if is_native_install(&probe.tool, path, real) {
        return RemoteCliSource::Native;
    }
    RemoteCliSource::Unknown
}

fn npm_package_dir(tool: &str, real: &str) -> bool {
    crate::commands::npm_install_args_for_tool(tool)
        .is_some_and(|(package, _)| real.contains(&format!("/node_modules/{package}/")))
}

fn is_native_install(tool: &str, path: &str, real: &str) -> bool {
    match tool {
        // ~/.local/bin/claude -> ~/.local/share/claude/versions/<v>
        "claude" => real.contains("/.local/share/claude/") || path.contains("/.claude/local/"),
        // ~/.grok/bin/grok -> ~/.grok/bin/grok-<v> or ~/.grok/downloads/grok-<platform>
        "grok" => real.contains("/.grok/bin/") || real.contains("/.grok/downloads/"),
        _ => false,
    }
}

/// `(is_cask, name)` from a Homebrew Cellar/Caskroom path.
fn brew_package(real: &str) -> Option<(bool, String)> {
    for (marker, cask) in [("/Cellar/", false), ("/Caskroom/", true)] {
        if let Some((_, rest)) = real.split_once(marker) {
            let name = rest.split('/').next().filter(|name| !name.is_empty())?;
            return Some((cask, name.to_string()));
        }
    }
    None
}

fn update_plan(probe: &Probe) -> Option<Plan> {
    let path = probe.path.as_deref()?;
    let real = probe.real.as_deref()?;
    match install_source(probe) {
        RemoteCliSource::Brew => brew_package(real).map(|(cask, name)| Plan::Brew { cask, name }),
        RemoteCliSource::Npm => Some(Plan::Npm {
            bin_dir: path
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or("")
                .to_string(),
        }),
        RemoteCliSource::Native => Some(Plan::SelfUpdate),
        // Claude's own updater knows every channel it was installed from.
        RemoteCliSource::Unknown if probe.tool == "claude" => Some(Plan::SelfUpdate),
        RemoteCliSource::Unknown | RemoteCliSource::Missing => None,
    }
}

fn update_command(probe: &Probe) -> Result<String, AppError> {
    let plan = update_plan(probe).ok_or_else(|| {
        AppError::Message(match probe.path.as_deref() {
            None => format!("远端未安装 {}", probe.tool),
            Some(path) => format!(
                "无法识别远端 {} 的安装方式（{}），请在服务器上手动更新",
                probe.tool,
                probe.real.as_deref().unwrap_or(path)
            ),
        })
    })?;
    let bin = shell_single_quote(probe.path.as_deref().unwrap_or(&probe.tool));
    Ok(match plan {
        Plan::Npm { bin_dir } => {
            let (package, extra) = crate::commands::npm_install_args_for_tool(&probe.tool)
                .ok_or_else(|| AppError::Message(format!("{} 没有 npm 包", probe.tool)))?;
            format!(
                "npm_bin={}/npm; [ -x \"$npm_bin\" ] || npm_bin=npm; \"$npm_bin\" i -g {package}@latest{extra}",
                shell_single_quote(&bin_dir)
            )
        }
        Plan::SelfUpdate if probe.tool == "grok" => {
            format!("{bin} update || {GROK_INSTALL_UNIX}")
        }
        Plan::SelfUpdate => format!("{bin} update"),
        Plan::Brew { cask, name } => format!(
            "brew upgrade {}{}",
            if cask { "--cask " } else { "" },
            shell_single_quote(&name)
        ),
    })
}

/// Runs `command` under the host's login PATH, capped at 15 minutes, and
/// reports its output and exit code instead of failing the SSH call.
fn update_script(command: &str) -> String {
    format!(
        "{REMOTE_PATH_PRELUDE}out=$( (limit 900 sh -c {}) </dev/null 2>&1 ); code=$?\nprintf '%s\\n' \"$out\"\nprintf '{EXIT_MARKER}%s\\n' \"$code\"\n",
        shell_single_quote(command)
    )
}

fn run_update(target: &ResolvedSshTarget, command: &str) -> Result<(), AppError> {
    let script = update_script(command);
    let output = run_ssh_command(target, "sh -s", Some(script.as_bytes()))?;
    let (log, code) = split_exit_code(&output)
        .ok_or_else(|| AppError::Message("远端更新命令没有返回结果".to_string()))?;
    if code == 0 {
        return Ok(());
    }
    Err(AppError::Message(update_failure_message(log, code)))
}

fn update_failure_message(log: &str, code: i32) -> String {
    let lines: Vec<&str> = log.lines().filter(|line| !line.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(FAILURE_TAIL_LINES)..].join("\n");
    if code == 124 {
        return format!("远端更新超时\n{tail}");
    }
    let hint = if log.contains("EACCES") {
        "\n远端 npm 全局目录需要管理员权限，请在服务器上用 sudo 更新，或改用 nvm 等用户级 Node"
    } else {
        ""
    };
    format!("远端更新失败（退出码 {code}）\n{tail}{hint}")
}

fn split_exit_code(output: &str) -> Option<(&str, i32)> {
    let index = output.rfind(EXIT_MARKER)?;
    let code = output[index + EXIT_MARKER.len()..].trim().parse().ok()?;
    Some((&output[..index], code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(tool: &str, path: Option<&str>, real: Option<&str>, version: &str) -> Probe {
        Probe {
            tool: tool.to_string(),
            path: path.map(str::to_string),
            real: real.map(str::to_string),
            version_output: version.to_string(),
        }
    }

    #[test]
    fn probe_output_skips_login_noise_and_keeps_missing_tools() {
        let output = "Welcome to Ubuntu\nUsing node v22\n\
            __CC_SWITCH_CLI__\tclaude\t/home/u/.local/bin/claude\t/home/u/.local/share/claude/versions/2.1.3\t2.1.3 (Claude Code)  \n\
            __CC_SWITCH_CLI__\tcodex\t\t\t\n";
        let probes = parse_probe_output(output);
        assert_eq!(
            probes,
            vec![
                probe_of(
                    "claude",
                    Some("/home/u/.local/bin/claude"),
                    Some("/home/u/.local/share/claude/versions/2.1.3"),
                    "2.1.3 (Claude Code)",
                ),
                probe_of("codex", None, None, ""),
            ]
        );
        let info = cli_info(&probes[1]);
        assert_eq!(info.source, RemoteCliSource::Missing);
        assert!(!info.updatable);
        assert!(info.error.is_none());
    }

    #[test]
    fn install_sources_follow_the_resolved_binary() {
        let cases = [
            (
                probe_of(
                    "codex",
                    Some("/home/u/.nvm/versions/node/v22/bin/codex"),
                    Some("/home/u/.nvm/versions/node/v22/lib/node_modules/@openai/codex/bin/codex.js"),
                    "codex-cli 0.160.1",
                ),
                RemoteCliSource::Npm,
            ),
            (
                // npm global of a Homebrew node is still npm.
                probe_of(
                    "gemini",
                    Some("/opt/homebrew/bin/gemini"),
                    Some("/opt/homebrew/lib/node_modules/@google/gemini-cli/dist/index.js"),
                    "0.9.0",
                ),
                RemoteCliSource::Npm,
            ),
            (
                probe_of(
                    "gemini",
                    Some("/home/linuxbrew/.linuxbrew/bin/gemini"),
                    Some("/home/linuxbrew/.linuxbrew/Cellar/gemini-cli/0.9.0/bin/gemini"),
                    "0.9.0",
                ),
                RemoteCliSource::Brew,
            ),
            (
                probe_of(
                    "claude",
                    Some("/home/u/.local/bin/claude"),
                    Some("/home/u/.local/share/claude/versions/2.1.3"),
                    "2.1.3",
                ),
                RemoteCliSource::Native,
            ),
            (
                probe_of(
                    "grok",
                    Some("/home/u/.grok/bin/grok"),
                    Some("/home/u/.grok/bin/grok-0.2.120"),
                    "grok 0.2.120",
                ),
                RemoteCliSource::Native,
            ),
            (
                probe_of("codex", Some("/usr/bin/codex"), Some("/usr/bin/codex"), "0.1.0"),
                RemoteCliSource::Unknown,
            ),
        ];
        for (probe, source) in cases {
            assert_eq!(install_source(&probe), source, "{probe:?}");
        }
    }

    #[test]
    fn update_commands_use_the_installs_own_channel() {
        let npm = probe_of(
            "claude",
            Some("/home/u/.nvm/versions/node/v22/bin/claude"),
            Some(
                "/home/u/.nvm/versions/node/v22/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            ),
            "2.1.3",
        );
        let command = update_command(&npm).unwrap();
        assert!(command.starts_with("npm_bin='/home/u/.nvm/versions/node/v22/bin'/npm;"));
        assert!(command.contains(
            "\"$npm_bin\" i -g @anthropic-ai/claude-code@latest --ignore-scripts=false --include=optional --allow-scripts=@anthropic-ai/claude-code"
        ));

        let native = probe_of(
            "claude",
            Some("/home/u/.local/bin/claude"),
            Some("/home/u/.local/share/claude/versions/2.1.3"),
            "2.1.3",
        );
        assert_eq!(
            update_command(&native).unwrap(),
            "'/home/u/.local/bin/claude' update"
        );

        let grok = probe_of(
            "grok",
            Some("/home/u/.grok/bin/grok"),
            Some("/home/u/.grok/bin/grok-0.2.120"),
            "0.2.120",
        );
        assert!(update_command(&grok)
            .unwrap()
            .starts_with("'/home/u/.grok/bin/grok' update || bash -c"));

        let brew = probe_of(
            "gemini",
            Some("/opt/homebrew/bin/gemini"),
            Some("/opt/homebrew/Cellar/gemini-cli/0.9.0/bin/gemini"),
            "0.9.0",
        );
        assert_eq!(update_command(&brew).unwrap(), "brew upgrade 'gemini-cli'");

        let unknown = probe_of(
            "codex",
            Some("/usr/bin/codex"),
            Some("/usr/bin/codex"),
            "0.1.0",
        );
        assert!(!cli_info(&unknown).updatable);
        let error = update_command(&unknown).unwrap_err().to_string();
        assert!(error.contains("无法识别远端 codex 的安装方式"), "{error}");

        let missing = probe_of("codex", None, None, "");
        assert!(update_command(&missing)
            .unwrap_err()
            .to_string()
            .contains("远端未安装 codex"));
    }

    #[test]
    fn unknown_claude_installs_fall_back_to_its_own_updater() {
        let probe = probe_of(
            "claude",
            Some("/usr/bin/claude"),
            Some("/usr/bin/claude"),
            "2.1.3",
        );
        assert!(cli_info(&probe).updatable);
        assert_eq!(update_command(&probe).unwrap(), "'/usr/bin/claude' update");
    }

    #[test]
    fn broken_version_output_is_reported() {
        let probe = probe_of(
            "gemini",
            Some("/usr/local/bin/gemini"),
            Some("/usr/local/lib/node_modules/@google/gemini-cli/dist/index.js"),
            "SyntaxError: Unexpected token",
        );
        let info = cli_info(&probe);
        assert_eq!(info.version, None);
        assert_eq!(info.error.as_deref(), Some("SyntaxError: Unexpected token"));
    }

    #[test]
    fn update_failures_keep_the_tail_and_explain_permissions() {
        let log = (1..=20)
            .map(|i| format!("line {i}"))
            .chain(["npm error code EACCES".to_string()])
            .collect::<Vec<_>>()
            .join("\n");
        let message = update_failure_message(&log, 243);
        assert!(message.starts_with("远端更新失败（退出码 243）"));
        assert!(!message.contains("line 9\n"));
        assert!(message.contains("line 10\n"));
        assert!(message.contains("sudo"));
        assert!(update_failure_message("", 124).starts_with("远端更新超时"));
        assert_eq!(
            split_exit_code("ok\n__CC_SWITCH_EXIT__0\n"),
            Some(("ok\n", 0))
        );
        assert_eq!(split_exit_code("cut off"), None);
    }

    /// Runs the real probe and update scripts through the local `sh`, with fake
    /// npm/native installs in a temporary HOME.
    #[cfg(unix)]
    mod shell {
        use super::*;
        use std::fs;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::process::{Command, Stdio};

        fn executable(path: &Path, body: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn run(home: &Path, args: &[&str], script: &str) -> String {
            let mut child = Command::new("sh")
                .arg("-s")
                .arg("--")
                .args(args)
                .env("HOME", home)
                // A shell without rc files, so the login-PATH lookup is a no-op.
                .env("SHELL", "/bin/sh")
                .env("PATH", "/usr/bin:/bin")
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
            String::from_utf8(output.stdout).unwrap()
        }

        fn probe_local(home: &Path, tools: &[&str]) -> Vec<Probe> {
            let script = format!("{REMOTE_PATH_PRELUDE}{REMOTE_PROBE_SCRIPT}");
            parse_probe_output(&run(home, tools, &script))
        }

        #[test]
        fn probe_script_finds_user_installs_and_resolves_links() {
            let home = tempfile::tempdir().unwrap();
            let home = fs::canonicalize(home.path()).unwrap();

            // npm global under ~/.npm-global, linked like npm does.
            let package = home.join(".npm-global/lib/node_modules/@openai/codex/bin/codex.js");
            executable(&package, "echo 'codex-cli 0.150.0'");
            fs::create_dir_all(home.join(".npm-global/bin")).unwrap();
            std::os::unix::fs::symlink(
                "../lib/node_modules/@openai/codex/bin/codex.js",
                home.join(".npm-global/bin/codex"),
            )
            .unwrap();
            // Native Claude: ~/.local/bin/claude -> ~/.local/share/claude/versions/2.1.3
            let native = home.join(".local/share/claude/versions/2.1.3");
            executable(&native, "echo '2.1.3 (Claude Code)'");
            fs::create_dir_all(home.join(".local/bin")).unwrap();
            std::os::unix::fs::symlink(&native, home.join(".local/bin/claude")).unwrap();

            let probes = probe_local(&home, &["claude", "codex", "cc-switch-missing-cli"]);
            assert_eq!(probes.len(), 3);

            let claude = cli_info(&probes[0]);
            assert_eq!(claude.version.as_deref(), Some("2.1.3"));
            assert_eq!(claude.source, RemoteCliSource::Native);
            assert_eq!(probes[0].real.as_deref(), native.to_str());

            let codex = cli_info(&probes[1]);
            assert_eq!(codex.version.as_deref(), Some("0.150.0"));
            assert_eq!(codex.source, RemoteCliSource::Npm);
            assert_eq!(probes[1].real.as_deref(), package.to_str());

            assert_eq!(cli_info(&probes[2]).source, RemoteCliSource::Missing);
        }

        #[test]
        fn npm_update_runs_the_npm_next_to_the_binary() {
            let home = tempfile::tempdir().unwrap();
            let home = fs::canonicalize(home.path()).unwrap();
            let bin = home.join(".npm-global/bin");
            let package =
                home.join(".npm-global/lib/node_modules/@google/gemini-cli/dist/index.js");
            executable(&package, "echo 0.9.0");
            fs::create_dir_all(&bin).unwrap();
            std::os::unix::fs::symlink(&package, bin.join("gemini")).unwrap();
            let log = home.join("npm-args");
            executable(
                &bin.join("npm"),
                &format!("echo \"$@\" > '{}'; echo installed", log.display()),
            );

            let probe = probe_local(&home, &["gemini"]).pop().unwrap();
            let command = update_command(&probe).unwrap();
            let script = update_script(&command);
            let output = run(&home, &[], &script);
            assert_eq!(split_exit_code(&output).map(|(_, code)| code), Some(0));
            assert_eq!(
                fs::read_to_string(log).unwrap().trim(),
                "i -g @google/gemini-cli@latest"
            );
        }

        #[test]
        fn failing_update_reports_its_exit_code() {
            let home = tempfile::tempdir().unwrap();
            let script = update_script("echo 'npm error code EACCES'; exit 243");
            let output = run(home.path(), &[], &script);
            let (log, code) = split_exit_code(&output).unwrap();
            assert_eq!(code, 243);
            assert!(update_failure_message(log, code).contains("sudo"));
        }
    }
}
