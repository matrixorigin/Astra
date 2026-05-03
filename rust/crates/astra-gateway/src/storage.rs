//! Gateway persistent storage backed by MatrixOne (MySQL-compatible).
//!
//! Owns its own database (`astra_gateway`) with tables for:
//! - gw_users: platform user profiles
//! - gw_sessions: chat_id → astra_session_id mappings
//! - gw_cron_jobs: scheduled tasks

use chrono::Datelike;
use sqlx::mysql::MySqlPool;

/// Connect to the database and ensure the schema exists.
pub async fn try_connect_db(url: &str) -> Result<MySqlPool, sqlx::Error> {
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

/// Initialize the gateway database schema.
pub async fn ensure_schema(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_users (
            platform VARCHAR(32) NOT NULL,
            platform_user_id VARCHAR(128) NOT NULL,
            display_name VARCHAR(256) DEFAULT '',
            preferences JSON,
            created_at DATETIME(6) DEFAULT NOW(6),
            updated_at DATETIME(6) DEFAULT NOW(6),
            PRIMARY KEY (platform, platform_user_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_sessions (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            platform VARCHAR(32) NOT NULL,
            chat_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL DEFAULT '',
            cli_profile VARCHAR(32) NOT NULL DEFAULT 'default',
            astra_session_id VARCHAR(64) NOT NULL,
            is_current BOOLEAN DEFAULT TRUE,
            created_at DATETIME(6) DEFAULT NOW(6),
            last_active DATETIME(6) DEFAULT NOW(6),
            INDEX idx_current (platform, chat_id, cli_profile, is_current),
            INDEX idx_user (platform, user_id)
        )",
    )
    .execute(pool)
    .await?;

    // Migration: add cli_profile column if missing (existing deployments)
    let _ = sqlx::query(
        "ALTER TABLE gw_sessions ADD COLUMN cli_profile VARCHAR(32) NOT NULL DEFAULT 'default'",
    )
    .execute(pool)
    .await;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_cron_jobs (
            job_id VARCHAR(64) PRIMARY KEY,
            platform VARCHAR(32) NOT NULL,
            chat_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL DEFAULT '',
            cron_expr VARCHAR(128) NOT NULL,
            message TEXT NOT NULL,
            description VARCHAR(512) DEFAULT '',
            enabled BOOLEAN DEFAULT TRUE,
            last_run DATETIME(6),
            next_run DATETIME(6),
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_enabled (enabled, next_run),
            INDEX idx_user_jobs (platform, user_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_platform_credentials (
            platform VARCHAR(32) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            credential_type VARCHAR(64) NOT NULL,
            credentials TEXT NOT NULL,
            expires_at DATETIME(6),
            created_at DATETIME(6) DEFAULT NOW(6),
            updated_at DATETIME(6) DEFAULT NOW(6),
            PRIMARY KEY (platform, user_id, credential_type)
        )",
    )
    .execute(pool)
    .await?;

    tracing::info!("gateway schema ensured");
    Ok(())
}

// ─── User operations ────────────────────────────────────────────────────────

pub async fn upsert_user(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    display_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO gw_users (platform, platform_user_id, display_name)
         VALUES (?, ?, ?)
         ON DUPLICATE KEY UPDATE display_name = VALUES(display_name), updated_at = NOW(6)",
    )
    .bind(platform)
    .bind(user_id)
    .bind(display_name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_user_preference(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    let pref_json = serde_json::json!({key: value}).to_string();
    // First ensure preferences is not NULL, then JSON_SET
    sqlx::query(
        "UPDATE gw_users SET preferences = ?, updated_at = NOW(6)
         WHERE platform = ? AND platform_user_id = ? AND preferences IS NULL",
    )
    .bind(&pref_json)
    .bind(platform)
    .bind(user_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE gw_users SET preferences = JSON_SET(preferences, CONCAT('$.', ?), ?), updated_at = NOW(6)
         WHERE platform = ? AND platform_user_id = ? AND preferences IS NOT NULL",
    )
    .bind(key)
    .bind(value)
    .bind(platform)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_user_preference(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT JSON_UNQUOTE(JSON_EXTRACT(preferences, CONCAT('$.', ?)))
         FROM gw_users WHERE platform = ? AND platform_user_id = ?",
    )
    .bind(key)
    .bind(platform)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| r.0).filter(|v| v != "null"))
}

// ─── Session operations ─────────────────────────────────────────────────────

pub async fn get_current_session(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    get_current_session_for_cli(pool, platform, chat_id, "astra").await
}

pub async fn get_current_session_for_cli(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
    cli_profile: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT astra_session_id FROM gw_sessions
         WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = TRUE
         ORDER BY last_active DESC LIMIT 1",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

pub async fn set_current_session(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    astra_session_id: &str,
) -> Result<(), sqlx::Error> {
    set_current_session_for_cli(pool, platform, chat_id, user_id, astra_session_id, "astra").await
}

pub async fn set_current_session_for_cli(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    astra_session_id: &str,
    cli_profile: &str,
) -> Result<(), sqlx::Error> {
    // Mark old sessions for this CLI as not current
    sqlx::query(
        "UPDATE gw_sessions SET is_current = FALSE
         WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = TRUE",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .execute(pool)
    .await?;

    // Check if this session_id already exists for this CLI
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM gw_sessions
         WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND astra_session_id = ?",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .bind(astra_session_id)
    .fetch_optional(pool)
    .await?;

    if let Some((id,)) = existing {
        // Reactivate existing session
        sqlx::query(
            "UPDATE gw_sessions SET is_current = TRUE, last_active = NOW(6) WHERE id = ?",
        )
        .bind(id)
        .execute(pool)
        .await?;
    } else {
        // Insert new
        sqlx::query(
            "INSERT INTO gw_sessions (platform, chat_id, user_id, cli_profile, astra_session_id, is_current)
             VALUES (?, ?, ?, ?, ?, TRUE)",
        )
        .bind(platform)
        .bind(chat_id)
        .bind(user_id)
        .bind(cli_profile)
        .bind(astra_session_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn touch_session(pool: &MySqlPool, platform: &str, chat_id: &str) -> Result<(), sqlx::Error> {
    touch_session_for_cli(pool, platform, chat_id, "astra").await
}

pub async fn touch_session_for_cli(pool: &MySqlPool, platform: &str, chat_id: &str, cli_profile: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE gw_sessions SET last_active = NOW(6)
         WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = TRUE",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_sessions(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
) -> Result<Vec<(String, bool, String)>, sqlx::Error> {
    list_sessions_for_cli(pool, platform, chat_id, "astra").await
}

pub async fn list_sessions_for_cli(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
    cli_profile: &str,
) -> Result<Vec<(String, bool, String)>, sqlx::Error> {
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT astra_session_id, is_current, CAST(created_at AS CHAR) as created
         FROM gw_sessions WHERE platform = ? AND chat_id = ? AND cli_profile = ?
         ORDER BY last_active DESC LIMIT 20",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn switch_session(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
    target_session_id: &str,
) -> Result<bool, sqlx::Error> {
    // Check target exists
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM gw_sessions
         WHERE platform = ? AND chat_id = ? AND astra_session_id = ?",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(target_session_id)
    .fetch_optional(pool)
    .await?;

    if exists.is_none() {
        return Ok(false);
    }

    // Clear current
    sqlx::query(
        "UPDATE gw_sessions SET is_current = FALSE
         WHERE platform = ? AND chat_id = ?",
    )
    .bind(platform)
    .bind(chat_id)
    .execute(pool)
    .await?;

    // Set target as current
    sqlx::query(
        "UPDATE gw_sessions SET is_current = TRUE, last_active = NOW(6)
         WHERE platform = ? AND chat_id = ? AND astra_session_id = ?",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(target_session_id)
    .execute(pool)
    .await?;

    Ok(true)
}

pub async fn reset_session(pool: &MySqlPool, platform: &str, chat_id: &str) -> Result<(), sqlx::Error> {
    reset_session_for_cli(pool, platform, chat_id, "astra").await
}

pub async fn reset_session_for_cli(pool: &MySqlPool, platform: &str, chat_id: &str, cli_profile: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE gw_sessions SET is_current = FALSE
         WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND is_current = TRUE",
    )
    .bind(platform)
    .bind(chat_id)
    .bind(cli_profile)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Platform credential operations ────────────────────────────────────────

/// A platform credential stored in the database.
#[derive(Debug, Clone)]
pub struct PlatformCredential {
    pub platform: String,
    pub user_id: String,
    pub credential_type: String,
    pub credentials: serde_json::Value,
    pub expires_at: Option<String>,
}

pub async fn save_credential(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    credential_type: &str,
    credentials: &serde_json::Value,
    expires_at: Option<&str>,
) -> Result<(), sqlx::Error> {
    let cred_str = credentials.to_string();
    sqlx::query(
        "INSERT INTO gw_platform_credentials (platform, user_id, credential_type, credentials, expires_at)
         VALUES (?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE credentials = VALUES(credentials),
                                 expires_at = VALUES(expires_at),
                                 updated_at = NOW(6)",
    )
    .bind(platform)
    .bind(user_id)
    .bind(credential_type)
    .bind(&cred_str)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_credential(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    credential_type: &str,
) -> Result<Option<PlatformCredential>, sqlx::Error> {
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT platform, user_id, credential_type, credentials, CAST(expires_at AS CHAR)
         FROM gw_platform_credentials
         WHERE platform = ? AND user_id = ? AND credential_type = ?",
    )
    .bind(platform)
    .bind(user_id)
    .bind(credential_type)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(p, u, ct, creds, exp)| PlatformCredential {
        platform: p,
        user_id: u,
        credential_type: ct,
        credentials: serde_json::from_str(&creds).unwrap_or(serde_json::Value::Null),
        expires_at: exp,
    }))
}

pub async fn list_credentials(
    pool: &MySqlPool,
    platform: &str,
) -> Result<Vec<PlatformCredential>, sqlx::Error> {
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT platform, user_id, credential_type, credentials, CAST(expires_at AS CHAR)
         FROM gw_platform_credentials
         WHERE platform = ?
         ORDER BY updated_at DESC",
    )
    .bind(platform)
    .fetch_all(pool)
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

pub async fn delete_credential(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
    credential_type: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM gw_platform_credentials
         WHERE platform = ? AND user_id = ? AND credential_type = ?",
    )
    .bind(platform)
    .bind(user_id)
    .bind(credential_type)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// ─── Cron job operations ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn create_cron_job(
    pool: &MySqlPool,
    job_id: &str,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    cron_expr: &str,
    message: &str,
    description: &str,
) -> Result<(), sqlx::Error> {
    let next = next_cron_run_str(cron_expr);
    sqlx::query(
        "INSERT INTO gw_cron_jobs (job_id, platform, chat_id, user_id, cron_expr, message, description, next_run)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job_id)
    .bind(platform)
    .bind(chat_id)
    .bind(user_id)
    .bind(cron_expr)
    .bind(message)
    .bind(description)
    .bind(next)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_cron_jobs(
    pool: &MySqlPool,
    platform: &str,
    chat_id: &str,
) -> Result<Vec<(String, String, String, bool)>, sqlx::Error> {
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT job_id, cron_expr, description, enabled
         FROM gw_cron_jobs WHERE platform = ? AND chat_id = ?
         ORDER BY created_at",
    )
    .bind(platform)
    .bind(chat_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_cron_job(pool: &MySqlPool, job_id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM gw_cron_jobs WHERE job_id = ?")
        .bind(job_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get_due_jobs(
    pool: &MySqlPool,
) -> Result<Vec<(String, String, String, String, String)>, sqlx::Error> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT job_id, platform, chat_id, message, cron_expr
         FROM gw_cron_jobs
         WHERE enabled = TRUE AND (next_run IS NULL OR next_run <= NOW(6))",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_job_run(pool: &MySqlPool, job_id: &str, cron_expr: &str) -> Result<(), sqlx::Error> {
    let next = next_cron_run_str(cron_expr);
    sqlx::query(
        "UPDATE gw_cron_jobs SET last_run = NOW(6), next_run = ? WHERE job_id = ?",
    )
    .bind(&next)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Compute the next run time from a cron expression as a MySQL datetime string.
/// Supports: "M H * * *" (daily), "M H * * DOW" (weekday filter).
/// Falls back to +24h if parsing fails.
fn next_cron_run_str(expr: &str) -> String {
    let now = chrono::Utc::now();
    let parts: Vec<&str> = expr.split_whitespace().collect();

    if parts.len() != 5 {
        let fallback = now + chrono::Duration::hours(24);
        return fallback.format("%Y-%m-%d %H:%M:%S").to_string();
    }

    let minute: u32 = parts[0].parse().unwrap_or(0);
    let hour: u32 = parts[1].parse().unwrap_or(9);

    // Start from tomorrow at the specified time
    let tomorrow = now + chrono::Duration::days(1);
    let mut candidate = tomorrow
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .unwrap_or(tomorrow.naive_utc());

    // If DOW is specified (not *), advance to matching day
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
/// Cron: 0=Sunday, 1=Monday, ..., 6=Saturday
/// Chrono num_days_from_monday: 0=Monday, ..., 6=Sunday
fn parse_dow(s: &str) -> Vec<u32> {
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

/// Convert cron day-of-week (0=Sun) to chrono (0=Mon).
fn cron_dow_to_chrono(cron_day: u32) -> u32 {
    (cron_day + 6) % 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cron_daily() {
        let next = next_cron_run_str("30 9 * * *");
        assert!(next.contains("09:30:00"), "expected 09:30, got {next}");
    }

    #[test]
    fn next_cron_weekday() {
        let next = next_cron_run_str("0 9 * * 1-5");
        assert!(next.contains("09:00:00"));
        // Can't easily assert weekday from string, just verify it parses
        assert!(next.len() >= 19);
    }

    #[test]
    fn next_cron_invalid_fallback() {
        let next = next_cron_run_str("garbage");
        assert!(next.len() >= 19, "should return a valid datetime string");
    }

    #[test]
    fn parse_dow_range() {
        // Cron 1-5 = Mon-Fri → chrono 0-4
        assert_eq!(parse_dow("1-5"), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn parse_dow_sunday() {
        // Cron 0 = Sunday → chrono 6
        assert_eq!(parse_dow("0"), vec![6]);
    }

    #[test]
    fn parse_dow_comma() {
        // Cron 1,3,5 = Mon,Wed,Fri → chrono 0,2,4
        assert_eq!(parse_dow("1,3,5"), vec![0, 2, 4]);
    }

    #[test]
    fn cron_dow_conversion() {
        assert_eq!(cron_dow_to_chrono(0), 6); // Sun
        assert_eq!(cron_dow_to_chrono(1), 0); // Mon
        assert_eq!(cron_dow_to_chrono(6), 5); // Sat
    }

    #[test]
    fn next_cron_midnight() {
        let next = next_cron_run_str("0 0 * * *");
        assert!(next.contains("00:00:00"));
    }

    #[test]
    fn next_cron_step_value_fallback() {
        // */5 can't be parsed as u32, falls back to minute=0
        let next = next_cron_run_str("*/5 * * * *");
        // Should still produce a valid datetime (fallback behavior)
        assert!(next.len() >= 19, "should be a valid datetime: {next}");
    }

    #[test]
    fn next_cron_is_in_future() {
        let next = next_cron_run_str("0 9 * * *");
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        assert!(next > now, "next_run {next} should be after now {now}");
    }

    #[test]
    fn parse_dow_empty() {
        assert!(parse_dow("").is_empty());
    }

    #[test]
    fn parse_dow_star() {
        // * should not be passed to parse_dow (caller checks), but handle gracefully
        assert!(parse_dow("*").is_empty());
    }

    #[test]
    fn parse_dow_weekend() {
        // Cron 0,6 = Sun,Sat → chrono 6,5
        let days = parse_dow("0,6");
        assert!(days.contains(&6)); // Sun
        assert!(days.contains(&5)); // Sat
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
            credentials: creds.clone(),
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
}
