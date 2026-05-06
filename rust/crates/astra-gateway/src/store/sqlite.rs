//! SQLite-backed [`GatewayStore`] implementation.
//!
//! Zero-config default backend — creates `~/.astra-gateway/gateway.db`
//! automatically on first use. All data types use TEXT for timestamps
//! and INTEGER for booleans, matching SQLite's type affinity model.

use super::{
    CronJobRecord, CronJobSpec, DueJob, GatewayStore, PendingMessage, PlatformCredential,
    SessionRecord, StoreError, UsageRecord, UsageSummary, next_cron_run_str,
};
use async_trait::async_trait;
use sqlx::SqlitePool;

/// SQLite-backed gateway store.
pub struct SqliteGatewayStore {
    pool: SqlitePool,
}

impl SqliteGatewayStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn connect(path: &str) -> Result<Self, StoreError> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let url = format!("sqlite:{path}?mode=rwc");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl GatewayStore for SqliteGatewayStore {
    // ── Schema ──────────────────────────────────────────────────────────

    async fn ensure_schema(&self) -> Result<(), StoreError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_users (
                platform TEXT NOT NULL,
                platform_user_id TEXT NOT NULL,
                display_name TEXT DEFAULT '',
                preferences TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now')),
                PRIMARY KEY (platform, platform_user_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                platform TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                user_id TEXT NOT NULL DEFAULT '',
                cli_profile TEXT NOT NULL DEFAULT 'default',
                astra_session_id TEXT NOT NULL,
                is_current INTEGER DEFAULT 1,
                created_at TEXT DEFAULT (datetime('now')),
                last_active TEXT DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_sessions_current
             ON gw_sessions(platform, chat_id, cli_profile, is_current)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_cron_jobs (
                job_id TEXT PRIMARY KEY,
                platform TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                user_id TEXT NOT NULL DEFAULT '',
                cron_expr TEXT NOT NULL,
                message TEXT NOT NULL,
                description TEXT DEFAULT '',
                enabled INTEGER DEFAULT 1,
                last_run TEXT,
                next_run TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_cron_enabled
             ON gw_cron_jobs(enabled, next_run)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_platform_credentials (
                platform TEXT NOT NULL,
                user_id TEXT NOT NULL,
                credential_type TEXT NOT NULL,
                credentials TEXT NOT NULL,
                expires_at TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now')),
                PRIMARY KEY (platform, user_id, credential_type)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_pending_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                platform TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at TEXT DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS gw_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                platform TEXT NOT NULL,
                user_id TEXT NOT NULL,
                cli_profile TEXT NOT NULL DEFAULT 'astra',
                model TEXT,
                tokens_prompt INTEGER NOT NULL DEFAULT 0,
                tokens_completion INTEGER NOT NULL DEFAULT 0,
                tool_calls INTEGER NOT NULL DEFAULT 0,
                elapsed_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_usage_user_day
             ON gw_usage(platform, user_id, created_at)",
        )
        .execute(&self.pool)
        .await?;

        tracing::info!("SQLite gateway schema ensured");
        Ok(())
    }

    // ── Users ───────────────────────────────────────────────────────────

    async fn is_first_message(&self, platform: &str, user_id: &str) -> Result<bool, StoreError> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM gw_users WHERE platform = ? AND platform_user_id = ?",
        )
        .bind(platform)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count == 0)
    }

    async fn upsert_user(
        &self,
        platform: &str,
        user_id: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO gw_users (platform, platform_user_id, display_name)
             VALUES (?, ?, ?)
             ON CONFLICT(platform, platform_user_id) DO UPDATE SET
                display_name = excluded.display_name,
                updated_at = datetime('now')",
        )
        .bind(platform)
        .bind(user_id)
        .bind(display_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        let pref_json = serde_json::json!({ key: value }).to_string();

        // First: if preferences is NULL, set the whole JSON object.
        let initialized = sqlx::query(
            "UPDATE gw_users SET preferences = ?, updated_at = datetime('now')
             WHERE platform = ? AND platform_user_id = ? AND preferences IS NULL",
        )
        .bind(&pref_json)
        .bind(platform)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        // Second: if preferences already exists, merge the key in.
        let merged = sqlx::query(
            "UPDATE gw_users
             SET preferences = json_set(preferences, '$.' || ?, ?),
                  updated_at = datetime('now')
             WHERE platform = ? AND platform_user_id = ? AND preferences IS NOT NULL",
        )
        .bind(key)
        .bind(value)
        .bind(platform)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if initialized.rows_affected() + merged.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "user not found: {platform}:{user_id}"
            )));
        }

        Ok(())
    }

    async fn get_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT json_extract(preferences, '$.' || ?)
             FROM gw_users WHERE platform = ? AND platform_user_id = ?",
        )
        .bind(key)
        .bind(platform)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        // SQLite json_extract returns NULL for missing keys (not the string "null").
        Ok(row.and_then(|r| r.0))
    }

    // ── Sessions ────────────────────────────────────────────────────────

    async fn get_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT astra_session_id FROM gw_sessions
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = 1
             ORDER BY last_active DESC LIMIT 1",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    async fn get_session_last_active(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT last_active FROM gw_sessions
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = 1
             ORDER BY last_active DESC LIMIT 1",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        astra_session_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        // Mark old sessions for this CLI as not current.
        sqlx::query(
            "UPDATE gw_sessions SET is_current = 0
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = 1",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .execute(&self.pool)
        .await?;

        // Check if this session_id already exists for this CLI.
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM gw_sessions
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND astra_session_id = ?",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .bind(astra_session_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((id,)) = existing {
            // Reactivate existing session.
            sqlx::query(
                "UPDATE gw_sessions SET is_current = 1, last_active = datetime('now') WHERE id = ?",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        } else {
            // Insert new session.
            sqlx::query(
                "INSERT INTO gw_sessions (platform, chat_id, user_id, cli_profile, astra_session_id, is_current)
                 VALUES (?, ?, ?, ?, ?, 1)",
            )
            .bind(platform)
            .bind(chat_id)
            .bind(user_id)
            .bind(cli_profile)
            .bind(astra_session_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn touch_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE gw_sessions SET last_active = datetime('now')
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = 1",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_sessions(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Vec<SessionRecord>, StoreError> {
        let rows: Vec<(String, i32, String)> = sqlx::query_as(
            "SELECT astra_session_id, is_current, created_at
             FROM gw_sessions WHERE platform = ? AND chat_id = ? AND cli_profile = ?
             ORDER BY last_active DESC LIMIT 20",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(sid, cur, created)| SessionRecord {
                session_id: sid,
                is_current: cur != 0,
                created_at: created,
            })
            .collect())
    }

    async fn switch_session(
        &self,
        platform: &str,
        chat_id: &str,
        target_session_id: &str,
    ) -> Result<bool, StoreError> {
        // Check target exists.
        let exists: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM gw_sessions
             WHERE platform = ? AND chat_id = ? AND astra_session_id = ?",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(target_session_id)
        .fetch_optional(&self.pool)
        .await?;

        if exists.is_none() {
            return Ok(false);
        }

        // Clear current.
        sqlx::query(
            "UPDATE gw_sessions SET is_current = 0
             WHERE platform = ? AND chat_id = ?",
        )
        .bind(platform)
        .bind(chat_id)
        .execute(&self.pool)
        .await?;

        // Set target as current.
        sqlx::query(
            "UPDATE gw_sessions SET is_current = 1, last_active = datetime('now')
             WHERE platform = ? AND chat_id = ? AND astra_session_id = ?",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(target_session_id)
        .execute(&self.pool)
        .await?;

        Ok(true)
    }

    async fn reset_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE gw_sessions SET is_current = 0
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = 1",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(cli_profile)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Cron jobs ───────────────────────────────────────────────────────

    async fn create_cron_job(&self, spec: &CronJobSpec) -> Result<(), StoreError> {
        let next = next_cron_run_str(&spec.cron_expr);
        sqlx::query(
            "INSERT INTO gw_cron_jobs (job_id, platform, chat_id, user_id, cron_expr, message, description, next_run)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&spec.job_id)
        .bind(&spec.platform)
        .bind(&spec.chat_id)
        .bind(&spec.user_id)
        .bind(&spec.cron_expr)
        .bind(&spec.message)
        .bind(&spec.description)
        .bind(&next)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_cron_jobs(
        &self,
        platform: &str,
        chat_id: &str,
    ) -> Result<Vec<CronJobRecord>, StoreError> {
        let rows: Vec<(String, String, String, i32)> = sqlx::query_as(
            "SELECT job_id, cron_expr, description, enabled
             FROM gw_cron_jobs WHERE platform = ? AND chat_id = ?
             ORDER BY created_at",
        )
        .bind(platform)
        .bind(chat_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, expr, desc, en)| CronJobRecord {
                job_id: id,
                cron_expr: expr,
                description: desc,
                enabled: en != 0,
            })
            .collect())
    }

    async fn delete_cron_job(&self, job_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM gw_cron_jobs WHERE job_id = ?")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_due_jobs(&self) -> Result<Vec<DueJob>, StoreError> {
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT job_id, platform, chat_id, message, cron_expr
             FROM gw_cron_jobs
             WHERE enabled = 1 AND (next_run IS NULL OR next_run <= datetime('now'))",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, plat, chat, msg, expr)| DueJob {
                job_id: id,
                platform: plat,
                chat_id: chat,
                message: msg,
                cron_expr: expr,
            })
            .collect())
    }

    async fn mark_job_run(&self, job_id: &str, cron_expr: &str) -> Result<(), StoreError> {
        let next = next_cron_run_str(cron_expr);
        sqlx::query(
            "UPDATE gw_cron_jobs SET last_run = datetime('now'), next_run = ? WHERE job_id = ?",
        )
        .bind(&next)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_cron_next_run(&self, job_id: &str, next_run: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE gw_cron_jobs SET next_run = ? WHERE job_id = ?")
            .bind(next_run)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_cron_job_user_id(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT user_id FROM gw_cron_jobs WHERE job_id = ?")
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(uid,)| uid))
    }

    // ── Platform credentials ────────────────────────────────────────────

    async fn save_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
        credentials: &serde_json::Value,
        expires_at: Option<&str>,
    ) -> Result<(), StoreError> {
        let cred_str = credentials.to_string();
        sqlx::query(
            "INSERT INTO gw_platform_credentials (platform, user_id, credential_type, credentials, expires_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(platform, user_id, credential_type) DO UPDATE SET
                credentials = excluded.credentials,
                expires_at = excluded.expires_at,
                updated_at = datetime('now')",
        )
        .bind(platform)
        .bind(user_id)
        .bind(credential_type)
        .bind(&cred_str)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<Option<PlatformCredential>, StoreError> {
        let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT platform, user_id, credential_type, credentials, expires_at
             FROM gw_platform_credentials
             WHERE platform = ? AND user_id = ? AND credential_type = ?",
        )
        .bind(platform)
        .bind(user_id)
        .bind(credential_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(p, u, ct, creds, exp)| PlatformCredential {
            platform: p,
            user_id: u,
            credential_type: ct,
            credentials: serde_json::from_str(&creds).unwrap_or(serde_json::Value::Null),
            expires_at: exp,
        }))
    }

    async fn list_credentials(
        &self,
        platform: &str,
    ) -> Result<Vec<PlatformCredential>, StoreError> {
        let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT platform, user_id, credential_type, credentials, expires_at
             FROM gw_platform_credentials
             WHERE platform = ?
             ORDER BY updated_at DESC",
        )
        .bind(platform)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(p, u, ct, creds, exp)| PlatformCredential {
                platform: p,
                user_id: u,
                credential_type: ct,
                credentials: serde_json::from_str(&creds).unwrap_or(serde_json::Value::Null),
                expires_at: exp,
            })
            .collect())
    }

    async fn delete_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "DELETE FROM gw_platform_credentials
             WHERE platform = ? AND user_id = ? AND credential_type = ?",
        )
        .bind(platform)
        .bind(user_id)
        .bind(credential_type)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Pending messages ────────────────────────────────────────────────

    async fn save_pending_message(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<i64, StoreError> {
        let result = sqlx::query(
            "INSERT INTO gw_pending_messages (platform, chat_id, user_id, text) VALUES (?, ?, ?, ?)",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(user_id)
        .bind(text)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn list_pending_messages(
        &self,
        platform: Option<&str>,
    ) -> Result<Vec<PendingMessage>, StoreError> {
        let rows: Vec<(i64, String, String, String, String)> = if let Some(plat) = platform {
            sqlx::query_as(
                "SELECT id, platform, chat_id, user_id, text
                 FROM gw_pending_messages
                 WHERE platform = ?
                 ORDER BY created_at
                 LIMIT 50",
            )
            .bind(plat)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, platform, chat_id, user_id, text
                 FROM gw_pending_messages
                 ORDER BY created_at
                 LIMIT 50",
            )
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(|(id, plat, chat, uid, txt)| PendingMessage {
                id,
                platform: plat,
                chat_id: chat,
                user_id: uid,
                text: txt,
            })
            .collect())
    }

    async fn delete_pending_message(&self, id: i64) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM gw_pending_messages WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // ── Usage ───────────────────────────────────────────────────────────

    async fn record_usage(&self, r: &UsageRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO gw_usage (platform, user_id, cli_profile, model, tokens_prompt, tokens_completion, tool_calls, elapsed_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.platform)
        .bind(&r.user_id)
        .bind(&r.cli_profile)
        .bind(&r.model)
        .bind(r.tokens_prompt as i64)
        .bind(r.tokens_completion as i64)
        .bind(r.tool_calls as i32)
        .bind(r.elapsed_ms as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_usage_today(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError> {
        let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*),
                    COALESCE(SUM(tokens_prompt), 0),
                    COALESCE(SUM(tokens_completion), 0),
                    COALESCE(SUM(tool_calls), 0)
             FROM gw_usage
             WHERE platform = ? AND user_id = ? AND created_at >= date('now')",
        )
        .bind(platform)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row
            .map(|(m, p, c, t)| UsageSummary {
                messages: m as u64,
                tokens_prompt: p as u64,
                tokens_completion: c as u64,
                tool_calls: t as u64,
            })
            .unwrap_or_default())
    }

    async fn get_usage_total(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError> {
        let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*),
                    COALESCE(SUM(tokens_prompt), 0),
                    COALESCE(SUM(tokens_completion), 0),
                    COALESCE(SUM(tool_calls), 0)
             FROM gw_usage
             WHERE platform = ? AND user_id = ?",
        )
        .bind(platform)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row
            .map(|(m, p, c, t)| UsageSummary {
                messages: m as u64,
                tokens_prompt: p as u64,
                tokens_completion: c as u64,
                tool_calls: t as u64,
            })
            .unwrap_or_default())
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteGatewayStore {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        let store = SqliteGatewayStore::new(pool);
        store.ensure_schema().await.expect("ensure_schema");
        store
    }

    #[tokio::test]
    async fn ensure_schema_runs_without_error() {
        let _ = make_store().await;
    }

    #[tokio::test]
    async fn ensure_schema_is_idempotent() {
        let store = make_store().await;
        // Running it again should not fail.
        store.ensure_schema().await.expect("second ensure_schema");
    }

    #[tokio::test]
    async fn upsert_user_and_is_first_message() {
        let store = make_store().await;

        assert!(store.is_first_message("wx", "u1").await.unwrap());

        store.upsert_user("wx", "u1", "Alice").await.unwrap();

        assert!(!store.is_first_message("wx", "u1").await.unwrap());

        // Other user is still first.
        assert!(store.is_first_message("wx", "u2").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_user_updates_display_name() {
        let store = make_store().await;
        store.upsert_user("wx", "u1", "Alice").await.unwrap();
        store.upsert_user("wx", "u1", "Bob").await.unwrap();

        // Still only one row.
        assert!(!store.is_first_message("wx", "u1").await.unwrap());
    }

    #[tokio::test]
    async fn set_and_get_user_preference() {
        let store = make_store().await;
        store.upsert_user("wx", "u1", "Test").await.unwrap();

        // No preference set yet.
        let val = store.get_user_preference("wx", "u1", "lang").await.unwrap();
        assert!(val.is_none());

        // Set one.
        store
            .set_user_preference("wx", "u1", "lang", "en")
            .await
            .unwrap();
        let val = store.get_user_preference("wx", "u1", "lang").await.unwrap();
        assert_eq!(val.as_deref(), Some("en"));

        // Overwrite.
        store
            .set_user_preference("wx", "u1", "lang", "zh")
            .await
            .unwrap();
        let val = store.get_user_preference("wx", "u1", "lang").await.unwrap();
        assert_eq!(val.as_deref(), Some("zh"));

        // Multiple keys coexist.
        store
            .set_user_preference("wx", "u1", "model_override_astra", "opus")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_user_preference("wx", "u1", "lang")
                .await
                .unwrap()
                .as_deref(),
            Some("zh")
        );
        assert_eq!(
            store
                .get_user_preference("wx", "u1", "model_override_astra")
                .await
                .unwrap()
                .as_deref(),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn get_preference_for_missing_user() {
        let store = make_store().await;
        let val = store
            .get_user_preference("wx", "ghost", "lang")
            .await
            .unwrap();
        assert!(val.is_none());
    }

    #[tokio::test]
    async fn set_preference_for_missing_user_fails() {
        let store = make_store().await;
        let err = store
            .set_user_preference("wx", "ghost", "lang", "en")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("user not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn create_and_list_cron_jobs() {
        let store = make_store().await;

        store
            .create_cron_job(&CronJobSpec {
                job_id: "j1".into(),
                platform: "wx".into(),
                chat_id: "c1".into(),
                user_id: "u1".into(),
                cron_expr: "30 9 * * *".into(),
                message: "good morning".into(),
                description: "daily greeting".into(),
            })
            .await
            .unwrap();

        store
            .create_cron_job(&CronJobSpec {
                job_id: "j2".into(),
                platform: "wx".into(),
                chat_id: "c1".into(),
                user_id: "u1".into(),
                cron_expr: "0 18 * * 1-5".into(),
                message: "wrap up".into(),
                description: "weekday reminder".into(),
            })
            .await
            .unwrap();

        let jobs = store.list_cron_jobs("wx", "c1").await.unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].job_id, "j1");
        assert!(jobs[0].enabled);
        assert_eq!(jobs[1].description, "weekday reminder");
    }

    #[tokio::test]
    async fn delete_cron_job() {
        let store = make_store().await;

        store
            .create_cron_job(&CronJobSpec {
                job_id: "j1".into(),
                platform: "wx".into(),
                chat_id: "c1".into(),
                user_id: "u1".into(),
                cron_expr: "0 9 * * *".into(),
                message: "hello".into(),
                description: "".into(),
            })
            .await
            .unwrap();

        assert!(store.delete_cron_job("j1").await.unwrap());
        assert!(!store.delete_cron_job("j1").await.unwrap()); // already deleted
        assert!(store.list_cron_jobs("wx", "c1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_message_roundtrip() {
        let store = make_store().await;

        let id1 = store
            .save_pending_message("wx", "c1", "u1", "hello")
            .await
            .unwrap();
        let id2 = store
            .save_pending_message("wx", "c1", "u1", "world")
            .await
            .unwrap();
        assert_ne!(id1, id2);

        // List all.
        let msgs = store.list_pending_messages(None).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "hello");
        assert_eq!(msgs[1].text, "world");

        // List filtered by platform.
        let msgs = store.list_pending_messages(Some("wx")).await.unwrap();
        assert_eq!(msgs.len(), 2);
        let msgs = store.list_pending_messages(Some("tg")).await.unwrap();
        assert!(msgs.is_empty());

        // Delete first.
        let affected = store.delete_pending_message(id1).await.unwrap();
        assert_eq!(affected, 1);

        let msgs = store.list_pending_messages(None).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "world");
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let store = make_store().await;

        // No current session initially.
        let cur = store
            .get_current_session("wx", "c1", "astra")
            .await
            .unwrap();
        assert!(cur.is_none());

        // Set a session.
        store
            .set_current_session("wx", "c1", "u1", "sess-1", "astra")
            .await
            .unwrap();
        let cur = store
            .get_current_session("wx", "c1", "astra")
            .await
            .unwrap();
        assert_eq!(cur.as_deref(), Some("sess-1"));

        // Touch.
        store.touch_session("wx", "c1", "astra").await.unwrap();

        // List.
        let sessions = store.list_sessions("wx", "c1", "astra").await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_current);

        // Set another session.
        store
            .set_current_session("wx", "c1", "u1", "sess-2", "astra")
            .await
            .unwrap();
        let cur = store
            .get_current_session("wx", "c1", "astra")
            .await
            .unwrap();
        assert_eq!(cur.as_deref(), Some("sess-2"));

        // Switch back.
        assert!(store.switch_session("wx", "c1", "sess-1").await.unwrap());
        let cur = store
            .get_current_session("wx", "c1", "astra")
            .await
            .unwrap();
        assert_eq!(cur.as_deref(), Some("sess-1"));

        // Switch to nonexistent.
        assert!(
            !store
                .switch_session("wx", "c1", "nonexistent")
                .await
                .unwrap()
        );

        // Reset.
        store.reset_session("wx", "c1", "astra").await.unwrap();
        let cur = store
            .get_current_session("wx", "c1", "astra")
            .await
            .unwrap();
        assert!(cur.is_none());
    }

    #[tokio::test]
    async fn credential_roundtrip() {
        let store = make_store().await;

        let creds = serde_json::json!({"token": "abc123"});
        store
            .save_credential("wx", "default", "bot_token", &creds, None)
            .await
            .unwrap();

        let got = store
            .get_credential("wx", "default", "bot_token")
            .await
            .unwrap()
            .expect("should exist");
        assert_eq!(got.credentials["token"], "abc123");
        assert!(got.expires_at.is_none());

        // Update.
        let new_creds = serde_json::json!({"token": "xyz789"});
        store
            .save_credential("wx", "default", "bot_token", &new_creds, Some("2026-12-31"))
            .await
            .unwrap();
        let got = store
            .get_credential("wx", "default", "bot_token")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.credentials["token"], "xyz789");
        assert_eq!(got.expires_at.as_deref(), Some("2026-12-31"));

        // List.
        let all = store.list_credentials("wx").await.unwrap();
        assert_eq!(all.len(), 1);

        // Delete.
        assert!(
            store
                .delete_credential("wx", "default", "bot_token")
                .await
                .unwrap()
        );
        assert!(
            !store
                .delete_credential("wx", "default", "bot_token")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn usage_roundtrip() {
        let store = make_store().await;

        // Initially zero.
        let today = store.get_usage_today("wx", "u1").await.unwrap();
        assert_eq!(today.messages, 0);

        store
            .record_usage(&UsageRecord {
                platform: "wx".into(),
                user_id: "u1".into(),
                cli_profile: "astra".into(),
                model: Some("opus".into()),
                tokens_prompt: 1000,
                tokens_completion: 200,
                tool_calls: 3,
                elapsed_ms: 5000,
            })
            .await
            .unwrap();

        let today = store.get_usage_today("wx", "u1").await.unwrap();
        assert_eq!(today.messages, 1);
        assert_eq!(today.tokens_prompt, 1000);
        assert_eq!(today.tokens_completion, 200);
        assert_eq!(today.tool_calls, 3);

        let total = store.get_usage_total("wx", "u1").await.unwrap();
        assert_eq!(total.messages, 1);
    }

    #[tokio::test]
    async fn update_cron_next_run_sets_timestamp() {
        let store = make_store().await;
        store
            .create_cron_job(&CronJobSpec {
                job_id: "j1".into(),
                platform: "wx".into(),
                chat_id: "c1".into(),
                user_id: "u1".into(),
                cron_expr: "0 9 * * *".into(),
                message: "hello".into(),
                description: "".into(),
            })
            .await
            .unwrap();

        store
            .update_cron_next_run("j1", "2099-12-31 23:59:59")
            .await
            .unwrap();

        // Verify via get_due_jobs: job should NOT be due (next_run far in future)
        let due = store.get_due_jobs().await.unwrap();
        assert!(due.is_empty(), "job with future next_run should not be due");
    }

    #[tokio::test]
    async fn session_last_active() {
        let store = make_store().await;

        let la = store
            .get_session_last_active("wx", "c1", "astra")
            .await
            .unwrap();
        assert!(la.is_none());

        store
            .set_current_session("wx", "c1", "u1", "sess-1", "astra")
            .await
            .unwrap();
        let la = store
            .get_session_last_active("wx", "c1", "astra")
            .await
            .unwrap();
        assert!(la.is_some());
    }

    #[tokio::test]
    async fn cli_profile_isolation() {
        let store = make_store().await;

        store
            .set_current_session("wx", "c1", "u1", "astra-sess", "astra")
            .await
            .unwrap();
        store
            .set_current_session("wx", "c1", "u1", "claude-sess", "claude")
            .await
            .unwrap();

        assert_eq!(
            store
                .get_current_session("wx", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("astra-sess")
        );
        assert_eq!(
            store
                .get_current_session("wx", "c1", "claude")
                .await
                .unwrap()
                .as_deref(),
            Some("claude-sess")
        );
    }
}
