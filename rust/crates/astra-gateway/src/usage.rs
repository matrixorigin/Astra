//! Usage tracking — per-user token/message/cost statistics.

use sqlx::MySqlPool;

pub async fn ensure_usage_table(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_usage (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            platform VARCHAR(32) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            cli_profile VARCHAR(32) NOT NULL DEFAULT 'astra',
            model VARCHAR(128),
            tokens_prompt BIGINT NOT NULL DEFAULT 0,
            tokens_completion BIGINT NOT NULL DEFAULT 0,
            tool_calls INT NOT NULL DEFAULT 0,
            elapsed_ms BIGINT NOT NULL DEFAULT 0,
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_user_day (platform, user_id, created_at)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

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

pub async fn record_usage(pool: &MySqlPool, r: &UsageRecord) -> Result<(), sqlx::Error> {
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
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug)]
pub struct UsageSummary {
    pub messages: u64,
    pub tokens_prompt: u64,
    pub tokens_completion: u64,
    pub tool_calls: u64,
}

pub async fn get_usage_today(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
) -> Result<UsageSummary, sqlx::Error> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(tokens_prompt),0), COALESCE(SUM(tokens_completion),0), COALESCE(SUM(tool_calls),0)
         FROM gw_usage WHERE platform = ? AND user_id = ? AND created_at >= CURDATE()",
    )
    .bind(platform)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|(m, p, c, t)| UsageSummary {
            messages: m as u64,
            tokens_prompt: p as u64,
            tokens_completion: c as u64,
            tool_calls: t as u64,
        })
        .unwrap_or(UsageSummary {
            messages: 0,
            tokens_prompt: 0,
            tokens_completion: 0,
            tool_calls: 0,
        }))
}

pub async fn get_usage_total(
    pool: &MySqlPool,
    platform: &str,
    user_id: &str,
) -> Result<UsageSummary, sqlx::Error> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(tokens_prompt),0), COALESCE(SUM(tokens_completion),0), COALESCE(SUM(tool_calls),0)
         FROM gw_usage WHERE platform = ? AND user_id = ?",
    )
    .bind(platform)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|(m, p, c, t)| UsageSummary {
            messages: m as u64,
            tokens_prompt: p as u64,
            tokens_completion: c as u64,
            tool_calls: t as u64,
        })
        .unwrap_or(UsageSummary {
            messages: 0,
            tokens_prompt: 0,
            tokens_completion: 0,
            tool_calls: 0,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_summary_default() {
        let s = UsageSummary {
            messages: 0,
            tokens_prompt: 0,
            tokens_completion: 0,
            tool_calls: 0,
        };
        assert_eq!(s.messages, 0);
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
}
