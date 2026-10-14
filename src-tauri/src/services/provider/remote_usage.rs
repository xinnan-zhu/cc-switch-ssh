//! SSH session usage synchronization. Remote collection is read-only; metadata
//! and cursors are committed atomically, with a ledger that outlives detail rows.
use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::remote::{
    configure_ssh_command, configure_ssh_password, resolve_ssh_target, InFlightGuard,
};
use super::SshConnectionTarget;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::{CostCalculator, TokenUsage};
use crate::services::session_usage::SessionSyncResult;
use crate::services::usage_stats::find_model_pricing;

const COLLECTOR: &str = include_str!("remote_usage_collector.py");
const APPS: &[&str] = &["claude", "codex", "gemini", "grokbuild"];
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteUsageConfig {
    pub enabled: bool,
    #[serde(default)]
    pub all_history: bool,
    /// Directories containing session logs, per app. Empty map uses defaults.
    #[serde(default)]
    pub roots: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteUsageHost {
    pub host_key: String,
    pub config: RemoteUsageConfig,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Event {
    app: String,
    id: String,
    session: String,
    model: String,
    timestamp: i64,
    input: u32,
    output: u32,
    cache_read: u32,
    cache_write: u32,
    #[serde(default)]
    cache_1h: u32,
    upstream_id: Option<String>,
}

#[derive(Deserialize)]
struct Collection {
    events: Vec<Event>,
    cursors: Value,
    files_scanned: u32,
    errors: Vec<String>,
    more: bool,
}

pub struct RemoteUsageService;

impl RemoteUsageService {
    pub fn hosts(db: &Database) -> Result<Vec<RemoteUsageHost>, AppError> {
        let conn = lock_conn!(db.conn);
        let mut stmt = conn.prepare("SELECT host_key, config_json, last_sync_at, last_error FROM remote_usage_hosts
            UNION ALL SELECT host_key, '{\"enabled\":false,\"allHistory\":false,\"roots\":{}}', NULL, NULL FROM remote_gateways
            WHERE host_key NOT IN (SELECT host_key FROM remote_usage_hosts) ORDER BY host_key")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?;
        rows.map(|row| {
            let (host_key, config, last_sync_at, last_error) = row?;
            Ok(RemoteUsageHost {
                host_key,
                config: serde_json::from_str(&config).map_err(json_error)?,
                last_sync_at,
                last_error,
            })
        })
        .collect()
    }

    pub fn configure(
        db: &Database,
        target: &SshConnectionTarget,
        config: RemoteUsageConfig,
    ) -> Result<(), AppError> {
        let resolved = resolve_ssh_target(target)?;
        if config.enabled && resolved.password.is_some() {
            return Err(AppError::Message(
                "自动用量同步需要 SSH 密钥登录；密码不会保存到数据库".into(),
            ));
        }
        for (app, roots) in &config.roots {
            if !APPS.contains(&app.as_str())
                || roots.len() > 16
                || roots
                    .iter()
                    .any(|p| p.is_empty() || p.len() > 4096 || p.contains('\0'))
            {
                return Err(AppError::Message("用量日志目录无效".into()));
            }
        }
        let _guard = InFlightGuard::acquire(format!("usage:{}", resolved.label))
            .ok_or_else(|| AppError::Message("该主机正在同步用量，请稍后再修改设置".into()))?;
        let conn = lock_conn!(db.conn);
        let since = if config.all_history {
            0
        } else {
            chrono::Utc::now().timestamp() - 30 * 86400
        };
        conn.execute("INSERT INTO remote_usage_hosts(host_key, target_json, config_json, since_at) VALUES(?1, ?2, ?3, ?4)
            ON CONFLICT(host_key) DO UPDATE SET target_json=excluded.target_json, config_json=excluded.config_json,
            cursors_json=CASE WHEN remote_usage_hosts.config_json <> excluded.config_json THEN '{}' ELSE remote_usage_hosts.cursors_json END,
            since_at=CASE WHEN remote_usage_hosts.config_json <> excluded.config_json THEN excluded.since_at ELSE remote_usage_hosts.since_at END",
            params![resolved.label, serde_json::to_string(target).map_err(json_error)?, serde_json::to_string(&config).map_err(json_error)?, since])?;
        Ok(())
    }

    pub async fn sync(
        db: Arc<Database>,
        target: SshConnectionTarget,
    ) -> Result<SessionSyncResult, AppError> {
        let resolved = resolve_ssh_target(&target)?;
        let host = resolved.label.clone();
        let _guard = InFlightGuard::acquire(format!("usage:{host}"))
            .ok_or_else(|| AppError::Message("该主机正在同步用量".into()))?;
        let (config, cursors, since): (String, String, i64) = {
            let conn = lock_conn!(db.conn);
            conn.query_row("SELECT config_json, cursors_json, since_at FROM remote_usage_hosts WHERE host_key=?1", [&host],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
                .ok_or_else(|| AppError::Message("请先保存远端用量同步设置".into()))?
        };
        let config: RemoteUsageConfig = serde_json::from_str(&config).map_err(json_error)?;
        // Fail closed on damaged cursor state. Replaying pruned detail is unsafe.
        let cursors: Value = serde_json::from_str(&cursors).map_err(json_error)?;
        let payload =
            serde_json::json!({"roots": config.roots, "cursors": cursors, "since": since});
        let script = format!(
            "CONFIG = __import__('json').loads({})\n{COLLECTOR}",
            serde_json::to_string(&payload.to_string()).map_err(json_error)?
        );
        let result = async {
            let mut command = std::process::Command::new("ssh");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            configure_ssh_command(&mut command, &resolved, "python3 -");
            let _askpass = configure_ssh_password(&mut command, &resolved)?;
            let mut command = tokio::process::Command::from(command);
            command
                .kill_on_drop(true)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = command
                .spawn()
                .map_err(|e| AppError::Message(format!("启动 SSH 采集失败: {e}")))?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| AppError::Message("SSH stdin 不可用".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| AppError::Message("SSH stdout 不可用".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| AppError::Message("SSH stderr 不可用".into()))?;
            let operation = async {
                let write = async {
                    stdin.write_all(script.as_bytes()).await?;
                    stdin.shutdown().await
                };
                let read = async {
                    let mut data = Vec::new();
                    stdout.take(MAX_OUTPUT + 1).read_to_end(&mut data).await?;
                    Ok::<_, std::io::Error>(data)
                };
                let errors = async {
                    let mut data = Vec::new();
                    stderr.take(8192).read_to_end(&mut data).await?;
                    Ok::<_, std::io::Error>(data)
                };
                let (_, output, error, status) =
                    tokio::try_join!(write, read, errors, child.wait())
                        .map_err(|e| AppError::Message(format!("SSH 采集失败: {e}")))?;
                if output.len() as u64 > MAX_OUTPUT {
                    return Err(AppError::Message(
                        "采集数据超过 16 MiB，未提交断点，请缩小日志目录范围".into(),
                    ));
                }
                if !status.success() {
                    // Python exceptions can contain source records. Never return raw stderr.
                    let message =
                        if String::from_utf8_lossy(&error).contains("python3: command not found") {
                            "远端需要 Python 3"
                        } else {
                            "SSH 采集失败，请检查连接和远端 Python 3"
                        };
                    return Err(AppError::Message(message.into()));
                }
                serde_json::from_slice::<Collection>(&output)
                    .map_err(|_| AppError::Message("远端采集返回无效元数据，未提交断点".into()))
            };
            let collection = tokio::time::timeout(std::time::Duration::from_secs(75), operation)
                .await
                .map_err(|_| AppError::Message("远端采集超时，断点保留，可重试".into()))??;
            let db = db.clone();
            let host = host.clone();
            tauri::async_runtime::spawn_blocking(move || commit_collection(&db, &host, collection))
                .await
                .map_err(|e| AppError::Message(format!("远端用量入库失败: {e}")))?
        }
        .await;
        if let Err(error) = &result {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "UPDATE remote_usage_hosts SET last_error=?2 WHERE host_key=?1",
                params![host, error.to_string()],
            )?;
        }
        if result.as_ref().is_ok_and(|r| r.imported > 0) {
            crate::usage_events::notify_log_recorded();
        }
        result
    }

    pub async fn sync_enabled(db: Arc<Database>) {
        let targets: Result<Vec<SshConnectionTarget>, AppError> = (|| {
            let conn = lock_conn!(db.conn);
            let mut stmt = conn.prepare("SELECT target_json FROM remote_usage_hosts WHERE json_extract(config_json, '$.enabled')=1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| serde_json::from_str(&r?).map_err(json_error))
                .collect()
        })();
        match targets {
            Ok(targets) => {
                for target in targets {
                    if let Err(e) = Self::sync(db.clone(), target).await {
                        log::warn!("Remote usage sync failed: {e}");
                    }
                }
            }
            Err(e) => log::warn!("Remote usage settings read failed: {e}"),
        }
    }
}

fn commit_collection(
    db: &Database,
    host: &str,
    collection: Collection,
) -> Result<SessionSyncResult, AppError> {
    let mut conn = lock_conn!(db.conn);
    let tx = conn.transaction()?;
    let mut result = SessionSyncResult {
        files_scanned: collection.files_scanned,
        errors: collection.errors,
        deferred_files: u32::from(collection.more),
        ..Default::default()
    };
    for event in collection.events {
        if !APPS.contains(&event.app.as_str())
            || event.id.len() > 1024
            || event.model.len() > 256
            || event.session.len() > 1024
            || event.timestamp < 0
            || event.timestamp > chrono::Utc::now().timestamp() + 86400
        {
            return Err(AppError::Message("采集元数据无效，整批回滚".into()));
        }
        let source = format!("remote_session:{host}:{}", event.app);
        let request = format!(
            "ssh:{}",
            sha2::Sha256::digest(format!("{source}\0{}", event.id).as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let covered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM remote_usage_gateway_windows WHERE host_key=?1 AND app_type=?2 AND ?3>=start_at AND (end_at IS NULL OR ?3<=end_at))", params![host, event.app, event.timestamp], |r| r.get(0))?;
        let upstream_seen = if let Some(id) = &event.upstream_id {
            tx.query_row("SELECT EXISTS(SELECT 1 FROM remote_usage_gateway_ids WHERE host_key=?1 AND app_type=?2 AND message_id=?3)", params![host, event.app, id], |r| r.get::<_, bool>(0))?
        } else {
            false
        };
        // Explicit gateway ownership is required when client logs lack response
        // IDs (Codex/Grok). Never guess using equal token counts or nearby times.
        if covered || upstream_seen {
            result.skipped += 1;
            continue;
        }
        let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source=?1 AND request_id=?2)", params![source, request], |r| r.get(0))?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM proxy_request_logs WHERE request_id=?1)",
            [&request],
            |r| r.get(0),
        )?;
        if known && !exists {
            result.skipped += 1;
            continue;
        }
        let usage = TokenUsage {
            input_tokens: event.input,
            output_tokens: event.output,
            cache_read_tokens: event.cache_read,
            cache_creation_tokens: event.cache_write,
            cache_creation_1h_tokens: event.cache_1h,
            model: Some(event.model.clone()),
            message_id: None,
        };
        let pricing = find_model_pricing(&tx, &event.model);
        let costs = pricing
            .as_ref()
            .map(|p| CostCalculator::calculate_for_app(&event.app, &usage, p, Decimal::ONE));
        let numbers: [String; 5] = costs
            .map(|c| {
                [
                    c.input_cost.to_string(),
                    c.output_cost.to_string(),
                    c.cache_read_cost.to_string(),
                    c.cache_creation_cost.to_string(),
                    c.total_cost.to_string(),
                ]
            })
            .unwrap_or_else(|| std::array::from_fn(|_| "0".into()));
        let semantics = if event.app == "claude" { 2 } else { 1 };
        let changed = tx.execute("INSERT INTO proxy_request_logs(request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
            latency_ms, status_code, session_id, provider_type, created_at, data_source)
            VALUES(?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 0, 200, ?16, 'remote_session', ?17, ?18)
            ON CONFLICT(request_id) DO UPDATE SET input_tokens=excluded.input_tokens, output_tokens=excluded.output_tokens,
            cache_read_tokens=excluded.cache_read_tokens, cache_creation_tokens=excluded.cache_creation_tokens,
            input_cost_usd=excluded.input_cost_usd, output_cost_usd=excluded.output_cost_usd,
            cache_read_cost_usd=excluded.cache_read_cost_usd, cache_creation_cost_usd=excluded.cache_creation_cost_usd, total_cost_usd=excluded.total_cost_usd
            WHERE excluded.output_tokens>proxy_request_logs.output_tokens OR excluded.input_tokens>proxy_request_logs.input_tokens
            OR excluded.cache_read_tokens>proxy_request_logs.cache_read_tokens OR excluded.cache_creation_tokens>proxy_request_logs.cache_creation_tokens",
            params![request, format!("{host} / {} (SSH Session)", event.app), event.app, event.model, Some(&event.model), event.input, event.output, event.cache_read, event.cache_write, semantics,
                numbers[0], numbers[1], numbers[2], numbers[3], numbers[4], event.session, event.timestamp, source])?;
        tx.execute("INSERT OR IGNORE INTO session_usage_dedup(data_source, request_id, semantic_id, has_entry_id) VALUES(?1, ?2, ?2, 1)", params![source, request])?;
        if changed > 0 {
            result.imported += 1;
        } else {
            result.skipped += 1;
        }
    }
    tx.execute("UPDATE remote_usage_hosts SET cursors_json=?2, last_sync_at=?3, last_error=?4 WHERE host_key=?1", params![host, collection.cursors.to_string(), chrono::Utc::now().timestamp(), (!result.errors.is_empty()).then(|| result.errors.join("; "))])?;
    tx.commit()?;
    Ok(result)
}

use sha2::Digest;

fn json_error(_: serde_json::Error) -> AppError {
    AppError::Message("远端用量设置或断点 JSON 无效".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(id: &str, timestamp: i64) -> Collection {
        serde_json::from_value(serde_json::json!({
            "events": [{"app":"codex", "id":id, "session":"thread-1", "model":"gpt-5",
                "timestamp":timestamp, "input":100, "output":20, "cache_read":80, "cache_write":0}],
            "cursors":{"file":{"offset":100}}, "files_scanned":1, "errors":[], "more":false
        }))
        .unwrap()
    }

    fn setup_host(db: &Database, host: &str) {
        let conn = db.conn.lock().unwrap();
        conn.execute("INSERT INTO remote_usage_hosts(host_key,target_json,config_json,since_at) VALUES(?1,'{}','{}',0)", [host]).unwrap();
    }

    #[test]
    fn remote_replay_and_rollup_keep_durable_identity() {
        let db = Database::memory().unwrap();
        setup_host(&db, "server-a");
        let old = chrono::Utc::now().timestamp() - 40 * 86400;
        assert_eq!(
            commit_collection(&db, "server-a", collection("one", old))
                .unwrap()
                .imported,
            1
        );
        assert_eq!(
            commit_collection(&db, "server-a", collection("one", old))
                .unwrap()
                .imported,
            0
        );
        db.rollup_and_prune(30).unwrap();
        assert_eq!(
            commit_collection(&db, "server-a", collection("one", old))
                .unwrap()
                .imported,
            0
        );
        let summary = db
            .get_usage_summary_scoped(None, None, None, None, None, Some("host:server-a"))
            .unwrap();
        assert_eq!(summary.total_requests, 1);
        assert_eq!(summary.total_input_tokens, 20);
        assert_eq!(summary.total_cache_read_tokens, 80);
        assert_eq!(
            db.get_usage_summary_scoped(None, None, None, None, None, Some("local"))
                .unwrap()
                .total_requests,
            0
        );
    }

    #[test]
    fn identical_usage_on_different_hosts_is_not_deduplicated() {
        let db = Database::memory().unwrap();
        setup_host(&db, "server-a");
        setup_host(&db, "server-b");
        let now = chrono::Utc::now().timestamp();
        commit_collection(&db, "server-a", collection("same-id", now)).unwrap();
        commit_collection(&db, "server-b", collection("same-id", now)).unwrap();
        assert_eq!(
            db.get_usage_summary(None, None, None, None, None)
                .unwrap()
                .total_requests,
            2
        );
        for host in ["server-a", "server-b"] {
            let filters = crate::services::usage_stats::LogFilters {
                origin: Some(format!("host:{host}")),
                ..Default::default()
            };
            let logs = db.get_request_logs(&filters, 0, 20).unwrap();
            assert_eq!(logs.total, 1);
            assert_eq!(logs.data[0].latency_ms, 0);
            assert!(logs.data[0].first_token_ms.is_none());
        }
    }

    #[test]
    fn gateway_ownership_and_ids_are_scoped_to_host_and_app() {
        let db = Database::memory().unwrap();
        setup_host(&db, "server-a");
        setup_host(&db, "server-b");
        let now = chrono::Utc::now().timestamp();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute("INSERT INTO remote_usage_gateway_windows(host_key,app_type,start_at,end_at) VALUES('server-a','codex',?1,?2)", params![now - 10, now + 10]).unwrap();
            conn.execute(
                "INSERT INTO remote_usage_gateway_ids VALUES('server-b','claude','msg-exact')",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            commit_collection(&db, "server-a", collection("owned", now))
                .unwrap()
                .imported,
            0
        );
        assert_eq!(
            commit_collection(&db, "server-b", collection("owned", now))
                .unwrap()
                .imported,
            1
        );
        let mut c = collection("exact", now);
        c.events[0].upstream_id = Some("msg-exact".into());
        // Same ID on another app is a different request.
        assert_eq!(commit_collection(&db, "server-b", c).unwrap().imported, 1);
        let mut c = collection("exact-claude", now);
        c.events[0].app = "claude".into();
        c.events[0].upstream_id = Some("msg-exact".into());
        assert_eq!(commit_collection(&db, "server-b", c).unwrap().imported, 0);
    }

    #[test]
    fn bad_event_rolls_back_usage_and_cursors() {
        let db = Database::memory().unwrap();
        setup_host(&db, "server-a");
        let mut c = collection("valid", chrono::Utc::now().timestamp());
        let bad = collection(&"x".repeat(1025), chrono::Utc::now().timestamp())
            .events
            .remove(0);
        c.events.push(bad);
        assert!(commit_collection(&db, "server-a", c).is_err());
        assert_eq!(
            db.get_usage_summary(None, None, None, None, None)
                .unwrap()
                .total_requests,
            0
        );
        let conn = db.conn.lock().unwrap();
        let cursor: String = conn
            .query_row("SELECT cursors_json FROM remote_usage_hosts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cursor, "{}");
    }

    #[test]
    fn rollups_separate_gateway_and_local_use_of_same_provider() {
        let db = Database::memory().unwrap();
        let old = chrono::Utc::now().timestamp() - 40 * 86400;
        {
            let conn = db.conn.lock().unwrap();
            for source in ["proxy", "remote:server-a", "remote:server-b"] {
                conn.execute("INSERT INTO proxy_request_logs(request_id,provider_id,app_type,model,input_tokens,output_tokens,latency_ms,status_code,created_at,data_source)
                    VALUES(?1,'shared','codex','gpt-5',100,20,1000,200,?2,?1)", params![source, old]).unwrap();
            }
        }
        db.rollup_and_prune(30).unwrap();
        for origin in ["local", "host:server-a", "host:server-b"] {
            assert_eq!(
                db.get_usage_summary_scoped(None, None, None, None, None, Some(origin))
                    .unwrap()
                    .total_requests,
                1
            );
            assert_eq!(
                db.get_provider_stats_scoped(None, None, None, None, None, Some(origin))
                    .unwrap()[0]
                    .request_count,
                1
            );
            assert_eq!(
                db.get_model_stats_scoped(None, None, None, None, None, Some(origin))
                    .unwrap()[0]
                    .request_count,
                1
            );
            assert_eq!(
                db.get_usage_summary_by_app_scoped(None, None, None, None, Some(origin))
                    .unwrap()[0]
                    .summary
                    .total_requests,
                1
            );
        }
    }

    #[test]
    fn claude_partial_response_updates_without_adding_request() {
        let db = Database::memory().unwrap();
        setup_host(&db, "server-a");
        let now = chrono::Utc::now().timestamp();
        let mut first = collection("msg-1", now);
        first.events[0].app = "claude".into();
        first.events[0].output = 1;
        commit_collection(&db, "server-a", first).unwrap();
        let mut last = collection("msg-1", now);
        last.events[0].app = "claude".into();
        last.events[0].output = 200;
        commit_collection(&db, "server-a", last).unwrap();
        let summary = db.get_usage_summary(None, None, None, None, None).unwrap();
        assert_eq!(summary.total_requests, 1);
        assert_eq!(summary.total_output_tokens, 200);
        assert_eq!(summary.total_input_tokens, 100);
    }
}
