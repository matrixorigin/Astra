//! Abstract gateway storage layer.
//!
//! Defines the [`GatewayStore`] trait and shared domain types. Backend
//! implementations live in sub-modules:
//! - [`mysql`] -- MySQL / MatrixOne (production)
//! - [`sqlite`] -- SQLite (lightweight / single-node)
//! - [`file`] -- JSON files on disk (zero-dependency, single-user)

pub mod file;
pub mod mysql;
pub mod sqlite;

use async_trait::async_trait;
use chrono::Datelike;

// ─── Error type ────────────────────────────────────────────────────────────

/// Unified error type for storage backends.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Database(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Serialization(e.to_string())
    }
}

// ─── Domain types ──────────────────────────────────────────────────────────

/// A gateway session record mapping a platform chat to an astra session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub is_current: bool,
    pub created_at: String,
}

/// A message queued for async delivery (e.g. when the CLI is busy).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingMessage {
    pub id: i64,
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub text: String,
}

/// Parameters for creating a new cron job.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJobSpec {
    pub job_id: String,
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub cron_expr: String,
    pub message: String,
    pub description: String,
}

/// A cron job as returned by list operations (display-oriented subset).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJobRecord {
    pub job_id: String,
    pub cron_expr: String,
    pub description: String,
    pub enabled: bool,
}

/// A cron job that is due for execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DueJob {
    pub job_id: String,
    pub platform: String,
    pub chat_id: String,
    pub message: String,
    pub cron_expr: String,
}

/// Stored credentials for a platform integration (e.g. bot tokens, API keys).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlatformCredential {
    pub platform: String,
    pub user_id: String,
    pub credential_type: String,
    pub credentials: serde_json::Value,
    pub expires_at: Option<String>,
}

/// A single usage event to be recorded.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageRecord {
    pub platform: String,
    pub user_id: String,
    pub cli_profile: String,
    pub model: Option<String>,
    pub tokens_prompt: u64,
    pub tokens_completion: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
}

/// Aggregated usage statistics over a time period.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UsageSummary {
    pub messages: u64,
    pub tokens_prompt: u64,
    pub tokens_completion: u64,
    pub tool_calls: u64,
}

// ─── GatewayStore trait ────────────────────────────────────────────────────

/// Unified persistence interface for gateway state.
///
/// Every method takes `&self` (implementations hold their own connection pool
/// or directory handle) and returns `Result<T, StoreError>`.
///
/// Methods that operated on a specific CLI profile in the old `storage.rs` now
/// take an explicit `cli_profile: &str` parameter instead of relying on
/// `_for_cli` function name suffixes.
#[async_trait]
pub trait GatewayStore: Send + Sync + 'static {
    // ── Schema / lifecycle ──────────────────────────────────────────────

    /// Ensure all required tables / directories exist.
    async fn ensure_schema(&self) -> Result<(), StoreError>;

    // ── Users ───────────────────────────────────────────────────────────

    /// Returns `true` if the user has never been seen before (no row exists).
    async fn is_first_message(&self, platform: &str, user_id: &str) -> Result<bool, StoreError>;

    async fn upsert_user(
        &self,
        platform: &str,
        user_id: &str,
        display_name: &str,
    ) -> Result<(), StoreError>;

    async fn set_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError>;

    async fn get_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<String>, StoreError>;

    // ── Sessions ────────────────────────────────────────────────────────
    async fn get_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError>;

    async fn get_session_last_active(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError>;

    async fn set_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        astra_session_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError>;

    async fn touch_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError>;

    async fn list_sessions(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Vec<SessionRecord>, StoreError>;

    async fn switch_session(
        &self,
        platform: &str,
        chat_id: &str,
        target_session_id: &str,
    ) -> Result<bool, StoreError>;

    async fn reset_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError>;

    // ── Cron jobs ───────────────────────────────────────────────────────
    async fn create_cron_job(&self, spec: &CronJobSpec) -> Result<(), StoreError>;

    async fn list_cron_jobs(
        &self,
        platform: &str,
        chat_id: &str,
    ) -> Result<Vec<CronJobRecord>, StoreError>;

    async fn delete_cron_job(&self, job_id: &str) -> Result<bool, StoreError>;

    async fn get_due_jobs(&self) -> Result<Vec<DueJob>, StoreError>;

    async fn mark_job_run(&self, job_id: &str, cron_expr: &str) -> Result<(), StoreError>;

    /// Update the `next_run` timestamp for a specific cron job.
    ///
    /// Used by `remind_after` to set a one-shot fire time that doesn't come
    /// from a cron expression.
    async fn update_cron_next_run(&self, job_id: &str, next_run: &str) -> Result<(), StoreError>;

    /// Return the `user_id` that created a cron job, or `None` if not found.
    async fn get_cron_job_user_id(&self, job_id: &str) -> Result<Option<String>, StoreError>;

    // ── Platform credentials ────────────────────────────────────────────
    async fn save_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
        credentials: &serde_json::Value,
        expires_at: Option<&str>,
    ) -> Result<(), StoreError>;

    async fn get_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<Option<PlatformCredential>, StoreError>;

    async fn list_credentials(&self, platform: &str)
    -> Result<Vec<PlatformCredential>, StoreError>;

    async fn delete_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<bool, StoreError>;

    // ── Pending messages ────────────────────────────────────────────────
    async fn save_pending_message(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<i64, StoreError>;

    async fn list_pending_messages(
        &self,
        platform: Option<&str>,
    ) -> Result<Vec<PendingMessage>, StoreError>;

    async fn delete_pending_message(&self, id: i64) -> Result<u64, StoreError>;

    // ── Usage ───────────────────────────────────────────────────────────
    async fn record_usage(&self, record: &UsageRecord) -> Result<(), StoreError>;

    async fn get_usage_today(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError>;

    async fn get_usage_total(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError>;
}

// ─── Shared helpers ────────────────────────────────────────────────────────

/// Compute the next run time from a cron expression as a datetime string
/// (`YYYY-MM-DD HH:MM:SS`).
///
/// Supports: `M H * * *` (daily), `M H * * DOW` (weekday filter).
/// Falls back to +24 h if parsing fails.
pub fn next_cron_run_str(expr: &str) -> String {
    let now = chrono::Utc::now();
    let parts: Vec<&str> = expr.split_whitespace().collect();

    if parts.len() != 5 {
        let fallback = now + chrono::Duration::hours(24);
        return fallback.format("%Y-%m-%d %H:%M:%S").to_string();
    }

    let minute: u32 = parts[0].parse().unwrap_or(0);
    let hour: u32 = parts[1].parse().unwrap_or(9);

    let tomorrow = now + chrono::Duration::days(1);
    let mut candidate = tomorrow
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .unwrap_or(tomorrow.naive_utc());

    if parts[4] != "*" {
        let target_days = parse_dow(parts[4]);
        if !target_days.is_empty() {
            for _ in 0..8 {
                let weekday = candidate.weekday().num_days_from_monday();
                if target_days.contains(&weekday) {
                    break;
                }
                candidate += chrono::Duration::days(1);
            }
        }
    }

    candidate.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Parse cron DOW field and convert to chrono's Monday-based numbering.
///
/// Cron:  0 = Sunday, 1 = Monday, ..., 6 = Saturday.
/// Chrono `num_days_from_monday`: 0 = Monday, ..., 6 = Sunday.
pub fn parse_dow(s: &str) -> Vec<u32> {
    let mut days = Vec::new();
    for part in s.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(s), Ok(e)) = (start.parse::<u32>(), end.parse::<u32>()) {
                for d in s..=e {
                    days.push(cron_dow_to_chrono(d));
                }
            }
        } else if let Ok(d) = part.parse::<u32>() {
            days.push(cron_dow_to_chrono(d));
        }
    }
    days
}

/// Convert cron day-of-week (0 = Sun) to chrono (0 = Mon).
pub fn cron_dow_to_chrono(cron_day: u32) -> u32 {
    (cron_day + 6) % 7
}

/// Build a JSON-path-safe preference key for per-CLI model overrides.
pub fn model_preference_key(cli_name: &str) -> String {
    let safe: String = cli_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("model_override_{safe}")
}

// ─── Storage configuration ────────────────────────────────────────────────

/// Backend selection and connection parameters for gateway persistence.
///
/// Deserialized from the gateway config file via a `"backend"` tag:
///
/// ```yaml
/// storage:
///   backend: mysql
///   url: "mysql://root:111@127.0.0.1:6001/astra_gateway"
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum StorageConfig {
    Mysql {
        url: String,
    },
    Sqlite {
        #[serde(default = "default_sqlite_path")]
        path: String,
    },
    File {
        #[serde(default = "default_file_dir")]
        dir: String,
    },
    None,
}

fn default_sqlite_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/.astra-gateway/gateway.db")
}

fn default_file_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/.astra-gateway/data")
}

impl Default for StorageConfig {
    /// Auto-detect: if `GATEWAY_DATABASE_URL` is set, use MySQL; otherwise
    /// fall back to SQLite at `~/.astra-gateway/gateway.db`.
    fn default() -> Self {
        match std::env::var("GATEWAY_DATABASE_URL") {
            Ok(url) => Self::Mysql { url },
            Err(_) => Self::Sqlite {
                path: default_sqlite_path(),
            },
        }
    }
}

/// Factory: create the right store from config.
pub async fn open_store(
    config: &StorageConfig,
) -> Result<Option<Box<dyn GatewayStore>>, Box<dyn std::error::Error + Send + Sync>> {
    match config {
        StorageConfig::Mysql { url } => {
            let store = mysql::MysqlGatewayStore::connect(url).await?;
            store.ensure_schema().await?;
            Ok(Some(Box::new(store)))
        }
        StorageConfig::Sqlite { path } => {
            let store = sqlite::SqliteGatewayStore::connect(path).await?;
            store.ensure_schema().await?;
            Ok(Some(Box::new(store)))
        }
        StorageConfig::File { dir } => {
            let store = file::FileGatewayStore::open(dir).await?;
            Ok(Some(Box::new(store)))
        }
        StorageConfig::None => Ok(None),
    }
}

// ─── Store bundle ─────────────────────────────────────────────────────────

/// Everything the runner needs from the storage layer.
///
/// `durable_store` and `trace_repo` currently require a MySQL pool, so they
/// are only available when the backend is MySQL. SQLite / file backends set
/// them to `None`.
pub struct StoreBundle {
    pub store: std::sync::Arc<dyn GatewayStore>,
    pub durable_store: Option<std::sync::Arc<crate::durable_task_store::MysqlDurableTaskStore>>,
    pub trace_repo: Option<std::sync::Arc<crate::trace_model::MysqlTraceRepository>>,
    /// The raw MySQL pool (if applicable). Exposed so callers that still need
    /// low-level pool access (e.g. ad-hoc SQL in gateway actions, scheduler
    /// `resolve_job_user_id`) can use it without down-casting.
    pub mysql_pool: Option<sqlx::MySqlPool>,
}

/// Create a [`StoreBundle`] from config — or `None` for [`StorageConfig::None`].
///
/// For MySQL backends the bundle also provisions the durable-task store and
/// trace repository backed by the same connection pool.
pub async fn open_store_bundle(
    config: &StorageConfig,
) -> Result<Option<StoreBundle>, Box<dyn std::error::Error + Send + Sync>> {
    match config {
        StorageConfig::Mysql { url } => {
            let store = mysql::MysqlGatewayStore::connect(url).await?;
            store.ensure_schema().await?;
            store.ensure_usage_table().await?;
            let pool = store.pool().clone();

            // Durable tasks
            let durable = std::sync::Arc::new(
                crate::durable_task_store::MysqlDurableTaskStore::new(pool.clone()),
            );

            // Trace repository
            crate::trace_model::ensure_schema(&pool).await?;
            let trace =
                std::sync::Arc::new(crate::trace_model::MysqlTraceRepository::new(pool.clone()));

            Ok(Some(StoreBundle {
                store: std::sync::Arc::new(store),
                durable_store: Some(durable),
                trace_repo: Some(trace),
                mysql_pool: Some(pool),
            }))
        }
        StorageConfig::Sqlite { path } => {
            let store = sqlite::SqliteGatewayStore::connect(path).await?;
            store.ensure_schema().await?;
            Ok(Some(StoreBundle {
                store: std::sync::Arc::new(store),
                durable_store: None,
                trace_repo: None,
                mysql_pool: None,
            }))
        }
        StorageConfig::File { dir } => {
            let store = file::FileGatewayStore::open(dir).await?;
            Ok(Some(StoreBundle {
                store: std::sync::Arc::new(store),
                durable_store: None,
                trace_repo: None,
                mysql_pool: None,
            }))
        }
        StorageConfig::None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cron utilities ──────────────────────────────────────────────────

    #[test]
    fn next_cron_daily() {
        let next = next_cron_run_str("30 9 * * *");
        assert!(next.contains("09:30:00"), "expected 09:30, got {next}");
    }

    #[test]
    fn next_cron_weekday() {
        let next = next_cron_run_str("0 9 * * 1-5");
        assert!(next.contains("09:00:00"));
        assert!(next.len() >= 19);
    }

    #[test]
    fn next_cron_invalid_fallback() {
        let next = next_cron_run_str("garbage");
        assert!(next.len() >= 19, "should return a valid datetime string");
    }

    #[test]
    fn next_cron_midnight() {
        let next = next_cron_run_str("0 0 * * *");
        assert!(next.contains("00:00:00"));
    }

    #[test]
    fn next_cron_step_value_fallback() {
        let next = next_cron_run_str("*/5 * * * *");
        assert!(next.len() >= 19, "should be a valid datetime: {next}");
    }

    #[test]
    fn next_cron_is_in_future() {
        let next = next_cron_run_str("0 9 * * *");
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        assert!(next > now, "next_run {next} should be after now {now}");
    }

    #[test]
    fn parse_dow_range() {
        assert_eq!(parse_dow("1-5"), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn parse_dow_sunday() {
        assert_eq!(parse_dow("0"), vec![6]);
    }

    #[test]
    fn parse_dow_comma() {
        assert_eq!(parse_dow("1,3,5"), vec![0, 2, 4]);
    }

    #[test]
    fn parse_dow_empty() {
        assert!(parse_dow("").is_empty());
    }

    #[test]
    fn parse_dow_star() {
        assert!(parse_dow("*").is_empty());
    }

    #[test]
    fn parse_dow_weekend() {
        let days = parse_dow("0,6");
        assert!(days.contains(&6));
        assert!(days.contains(&5));
    }

    #[test]
    fn cron_dow_conversion() {
        assert_eq!(cron_dow_to_chrono(0), 6); // Sun
        assert_eq!(cron_dow_to_chrono(1), 0); // Mon
        assert_eq!(cron_dow_to_chrono(6), 5); // Sat
    }

    // ── model_preference_key ────────────────────────────────────────────

    #[test]
    fn model_override_key_basic() {
        assert_eq!(model_preference_key("astra"), "model_override_astra");
        assert_eq!(model_preference_key("claude"), "model_override_claude");
        assert_eq!(model_preference_key("codex"), "model_override_codex");
    }

    #[test]
    fn model_override_key_uses_underscore_separator() {
        for cli_name in &["astra", "claude", "codex"] {
            let key = model_preference_key(cli_name);
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "key must be alphanumeric + underscore only: {key}"
            );
        }
    }

    #[test]
    fn model_override_key_sanitizes_special_chars() {
        assert_eq!(
            model_preference_key("/opt/my-cli.v2"),
            "model_override__opt_my_cli_v2"
        );
        assert_eq!(
            model_preference_key("some:tool"),
            "model_override_some_tool"
        );
    }

    // ── StorageConfig ───────────────────────────────────────────────────

    #[test]
    fn storage_config_default_without_env() {
        let cfg = StorageConfig::default();
        match &cfg {
            StorageConfig::Mysql { url } => {
                assert!(!url.is_empty(), "MySQL URL should not be empty");
            }
            StorageConfig::Sqlite { path } => {
                assert!(
                    path.ends_with("gateway.db"),
                    "SQLite path should end with gateway.db, got: {path}"
                );
            }
            other => panic!("expected Mysql or Sqlite default, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_mysql() {
        let json = r#"{"backend":"mysql","url":"mysql://root@localhost/gw"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match cfg {
            StorageConfig::Mysql { url } => {
                assert_eq!(url, "mysql://root@localhost/gw");
            }
            other => panic!("expected Mysql, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_mysql_yaml() {
        let yaml = r#"backend: mysql
url: "mysql://root:111@localhost/gw""#;
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(cfg, StorageConfig::Mysql { .. }));
    }

    #[test]
    fn storage_config_deserialize_sqlite() {
        let json = r#"{"backend":"sqlite","path":"/tmp/test.db"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match cfg {
            StorageConfig::Sqlite { path } => {
                assert_eq!(path, "/tmp/test.db");
            }
            other => panic!("expected Sqlite, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_sqlite_default_path() {
        let json = r#"{"backend":"sqlite"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match cfg {
            StorageConfig::Sqlite { path } => {
                assert!(
                    path.ends_with("gateway.db"),
                    "default path should end with gateway.db, got: {path}"
                );
            }
            other => panic!("expected Sqlite, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_file() {
        let json = r#"{"backend":"file","dir":"/var/data/gw"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match cfg {
            StorageConfig::File { dir } => {
                assert_eq!(dir, "/var/data/gw");
            }
            other => panic!("expected File, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_file_default_dir() {
        let json = r#"{"backend":"file"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match cfg {
            StorageConfig::File { dir } => {
                assert!(
                    dir.ends_with("data"),
                    "default dir should end with 'data', got: {dir}"
                );
            }
            other => panic!("expected File, got: {other:?}"),
        }
    }

    #[test]
    fn storage_config_deserialize_none() {
        let json = r#"{"backend":"none"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(cfg, StorageConfig::None));
    }

    #[test]
    fn storage_config_deserialize_none_yaml() {
        let yaml = "backend: none";
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(cfg, StorageConfig::None));
    }

    // ── Domain type construction ────────────────────────────────────────

    #[test]
    fn session_record_fields() {
        let r = SessionRecord {
            session_id: "sess-001".into(),
            is_current: true,
            created_at: "2026-01-01 00:00:00".into(),
        };
        assert!(r.is_current);
        assert_eq!(r.session_id, "sess-001");
    }

    #[test]
    fn pending_message_fields() {
        let m = PendingMessage {
            id: 42,
            platform: "weixin".into(),
            chat_id: "chat-1".into(),
            user_id: "u-1".into(),
            text: "hello".into(),
        };
        assert_eq!(m.id, 42);
        assert_eq!(m.text, "hello");
    }

    #[test]
    fn cron_job_spec_fields() {
        let spec = CronJobSpec {
            job_id: "j-1".into(),
            platform: "telegram".into(),
            chat_id: "c-1".into(),
            user_id: "u-1".into(),
            cron_expr: "0 9 * * *".into(),
            message: "/status".into(),
            description: "daily check".into(),
        };
        assert_eq!(spec.cron_expr, "0 9 * * *");
    }

    #[test]
    fn cron_job_record_fields() {
        let rec = CronJobRecord {
            job_id: "j-2".into(),
            cron_expr: "0 0 * * 1".into(),
            description: "weekly".into(),
            enabled: false,
        };
        assert!(!rec.enabled);
    }

    #[test]
    fn due_job_fields() {
        let dj = DueJob {
            job_id: "j-3".into(),
            platform: "wecom".into(),
            chat_id: "c-2".into(),
            message: "run".into(),
            cron_expr: "*/5 * * * *".into(),
        };
        assert_eq!(dj.platform, "wecom");
    }

    #[test]
    fn platform_credential_roundtrip() {
        let creds = serde_json::json!({
            "token": "test-token-123",
            "account_id": "wx_abc"
        });
        let pc = PlatformCredential {
            platform: "weixin".into(),
            user_id: "default".into(),
            credential_type: "bot_token".into(),
            credentials: creds,
            expires_at: Some("2026-06-01 00:00:00".into()),
        };
        assert_eq!(pc.credentials["token"], "test-token-123");
        assert_eq!(pc.credentials["account_id"], "wx_abc");
        assert_eq!(pc.platform, "weixin");
        assert!(pc.expires_at.is_some());
    }

    #[test]
    fn platform_credential_no_expiry() {
        let pc = PlatformCredential {
            platform: "wecom".into(),
            user_id: "bot-1".into(),
            credential_type: "api_key".into(),
            credentials: serde_json::json!({"secret": "s3cr3t"}),
            expires_at: None,
        };
        assert!(pc.expires_at.is_none());
        assert_eq!(pc.credentials["secret"], "s3cr3t");
    }

    #[test]
    fn usage_record_fields() {
        let r = UsageRecord {
            platform: "weixin".into(),
            user_id: "u1".into(),
            cli_profile: "claude".into(),
            model: Some("opus".into()),
            tokens_prompt: 1000,
            tokens_completion: 200,
            tool_calls: 3,
            elapsed_ms: 5000,
        };
        assert_eq!(r.tokens_prompt + r.tokens_completion, 1200);
    }

    #[test]
    fn usage_summary_default() {
        let s = UsageSummary::default();
        assert_eq!(s.messages, 0);
        assert_eq!(s.tokens_prompt, 0);
        assert_eq!(s.tokens_completion, 0);
        assert_eq!(s.tool_calls, 0);
    }

    // ── StoreError ──────────────────────────────────────────────────────

    #[test]
    fn store_error_display() {
        let e = StoreError::Database("connection refused".into());
        assert_eq!(e.to_string(), "database error: connection refused");

        let e = StoreError::Serialization("invalid JSON".into());
        assert_eq!(e.to_string(), "serialization error: invalid JSON");
    }

    #[test]
    fn store_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let e: StoreError = io_err.into();
        assert!(matches!(e, StoreError::Io(_)));
        assert!(e.to_string().contains("file missing"));
    }

    #[test]
    fn store_error_from_serde_json() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let e: StoreError = json_err.into();
        assert!(matches!(e, StoreError::Serialization(_)));
    }

    // ── Serde round-trips (file backend compatibility) ──────────────────

    #[test]
    fn session_record_serde_roundtrip() {
        let original = SessionRecord {
            session_id: "s-1".into(),
            is_current: true,
            created_at: "2026-05-04 12:00:00".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.session_id, original.session_id);
        assert_eq!(decoded.is_current, original.is_current);
    }

    #[test]
    fn pending_message_serde_roundtrip() {
        let original = PendingMessage {
            id: 7,
            platform: "telegram".into(),
            chat_id: "c".into(),
            user_id: "u".into(),
            text: "msg with \"quotes\"".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PendingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.text, original.text);
    }

    #[test]
    fn credential_serde_roundtrip() {
        let original = PlatformCredential {
            platform: "weixin".into(),
            user_id: "u".into(),
            credential_type: "token".into(),
            credentials: serde_json::json!({"k": "v"}),
            expires_at: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PlatformCredential = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.credentials["k"], "v");
    }

    #[test]
    fn usage_record_serde_roundtrip() {
        let original = UsageRecord {
            platform: "wx".into(),
            user_id: "u1".into(),
            cli_profile: "astra".into(),
            model: Some("opus".into()),
            tokens_prompt: 500,
            tokens_completion: 100,
            tool_calls: 2,
            elapsed_ms: 3000,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: UsageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.tokens_prompt, 500);
        assert_eq!(decoded.model.as_deref(), Some("opus"));
    }

    // ── Cross-backend compliance ────────────────────────────────────────

    async fn compliance_test(store: &dyn GatewayStore) {
        // Schema
        store.ensure_schema().await.unwrap();

        // User lifecycle
        assert!(store.is_first_message("test", "u1").await.unwrap());
        store.upsert_user("test", "u1", "Alice").await.unwrap();
        assert!(!store.is_first_message("test", "u1").await.unwrap());

        // Preferences
        store
            .set_user_preference("test", "u1", "theme", "dark")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_user_preference("test", "u1", "theme")
                .await
                .unwrap()
                .as_deref(),
            Some("dark")
        );
        assert!(
            store
                .get_user_preference("test", "u1", "missing")
                .await
                .unwrap()
                .is_none()
        );

        // Sessions
        assert!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .is_none()
        );
        store
            .set_current_session("test", "c1", "u1", "s1", "astra")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("s1")
        );
        assert!(
            store
                .get_session_last_active("test", "c1", "astra")
                .await
                .unwrap()
                .is_some()
        );

        // Session switch
        store
            .set_current_session("test", "c1", "u1", "s2", "astra")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("s2")
        );
        let sessions = store.list_sessions("test", "c1", "astra").await.unwrap();
        assert_eq!(sessions.len(), 2);

        // Reset
        store.reset_session("test", "c1", "astra").await.unwrap();
        assert!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .is_none()
        );

        // CLI profile isolation
        store
            .set_current_session("test", "c1", "u1", "s3", "claude")
            .await
            .unwrap();
        assert!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .get_current_session("test", "c1", "claude")
                .await
                .unwrap()
                .as_deref(),
            Some("s3")
        );

        // Cron jobs
        let spec = CronJobSpec {
            job_id: "j-test".into(),
            platform: "test".into(),
            chat_id: "c1".into(),
            user_id: "u1".into(),
            cron_expr: "0 9 * * *".into(),
            message: "hello".into(),
            description: "desc".into(),
        };
        store.create_cron_job(&spec).await.unwrap();
        let jobs = store.list_cron_jobs("test", "c1").await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, "j-test");

        // update_cron_next_run (new method!)
        store
            .update_cron_next_run("j-test", "2099-12-31 23:59:59")
            .await
            .unwrap();

        // get_cron_job_user_id
        assert_eq!(
            store
                .get_cron_job_user_id("j-test")
                .await
                .unwrap()
                .as_deref(),
            Some("u1")
        );
        assert!(
            store
                .get_cron_job_user_id("nonexistent")
                .await
                .unwrap()
                .is_none()
        );

        // Delete
        assert!(store.delete_cron_job("j-test").await.unwrap());
        assert!(!store.delete_cron_job("j-test").await.unwrap());

        // Pending messages
        let id = store
            .save_pending_message("test", "c1", "u1", "hello")
            .await
            .unwrap();
        let msgs = store.list_pending_messages(Some("test")).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hello");
        store.delete_pending_message(id).await.unwrap();
        assert!(
            store
                .list_pending_messages(Some("test"))
                .await
                .unwrap()
                .is_empty()
        );

        // Credentials
        let creds = serde_json::json!({"key": "value"});
        store
            .save_credential("test", "u1", "token", &creds, None)
            .await
            .unwrap();
        let got = store
            .get_credential("test", "u1", "token")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.credentials["key"], "value");
        let list = store.list_credentials("test").await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(
            store
                .delete_credential("test", "u1", "token")
                .await
                .unwrap()
        );

        // switch_session
        store
            .set_current_session("test", "c1", "u1", "s10", "astra")
            .await
            .unwrap();
        store
            .set_current_session("test", "c1", "u1", "s11", "astra")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("s11")
        );
        assert!(store.switch_session("test", "c1", "s10").await.unwrap());
        assert_eq!(
            store
                .get_current_session("test", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("s10")
        );
        assert!(
            !store
                .switch_session("test", "c1", "nonexistent")
                .await
                .unwrap()
        );

        // Pending messages
        let id = store
            .save_pending_message("test", "c1", "u1", "hello")
            .await
            .unwrap();
        let msgs = store.list_pending_messages(Some("test")).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hello");
        store.delete_pending_message(id).await.unwrap();
        assert!(
            store
                .list_pending_messages(Some("test"))
                .await
                .unwrap()
                .is_empty()
        );

        // list_pending_messages with None (all platforms)
        let id_a = store
            .save_pending_message("plat_a", "c1", "u1", "msg_a")
            .await
            .unwrap();
        let id_b = store
            .save_pending_message("plat_b", "c1", "u1", "msg_b")
            .await
            .unwrap();
        let all = store.list_pending_messages(None).await.unwrap();
        assert!(
            all.len() >= 2,
            "should list from all platforms, got {}",
            all.len()
        );
        store.delete_pending_message(id_a).await.unwrap();
        store.delete_pending_message(id_b).await.unwrap();

        // Credentials
        let creds = serde_json::json!({"key": "value"});
        store
            .save_credential("test", "u1", "token", &creds, None)
            .await
            .unwrap();
        let got = store
            .get_credential("test", "u1", "token")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.credentials["key"], "value");
        let list = store.list_credentials("test").await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(
            store
                .delete_credential("test", "u1", "token")
                .await
                .unwrap()
        );

        // Usage
        let record = UsageRecord {
            platform: "test".into(),
            user_id: "u1".into(),
            cli_profile: "astra".into(),
            model: Some("opus".into()),
            tokens_prompt: 100,
            tokens_completion: 50,
            tool_calls: 2,
            elapsed_ms: 3000,
        };
        store.record_usage(&record).await.unwrap();
        let today = store.get_usage_today("test", "u1").await.unwrap();
        assert_eq!(today.messages, 1);
        assert_eq!(today.tokens_prompt, 100);

        // Usage accumulation
        store.record_usage(&record).await.unwrap();
        let total = store.get_usage_total("test", "u1").await.unwrap();
        assert_eq!(total.messages, 2);
        assert_eq!(total.tokens_prompt, 200);
    }

    #[tokio::test]
    async fn compliance_sqlite() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        let store = super::sqlite::SqliteGatewayStore::new(pool);
        store.ensure_schema().await.expect("ensure_schema");
        compliance_test(&store).await;
    }

    #[tokio::test]
    async fn compliance_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = super::file::FileGatewayStore::open(dir.path())
            .await
            .unwrap();
        compliance_test(&store).await;
    }

    #[test]
    fn cron_job_spec_serde_roundtrip() {
        let original = CronJobSpec {
            job_id: "j-1".into(),
            platform: "wx".into(),
            chat_id: "c-1".into(),
            user_id: "u-1".into(),
            cron_expr: "0 9 * * *".into(),
            message: "hi".into(),
            description: "desc".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: CronJobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.job_id, "j-1");
    }
}
