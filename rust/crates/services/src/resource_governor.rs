//! Per-user resource governance for multi-tenant web agent sessions.
//!
//! Tracks daily usage (sessions created, tool calls, tokens consumed) and enforces
//! configurable per-user limits.  Database-backed for production; in-memory for tests.
//!
//! # Tables
//!
//! * `resource_limits` — per-user override of default caps
//! * `resource_usage`  — daily aggregate counters keyed by `(user_id, usage_date)`
//!
//! # Design
//!
//! * **Fail-open**: DB errors log a warning and return defaults / proceed.
//! * **Atomic counters**: `ON DUPLICATE KEY UPDATE` for race-free increments.
//! * **Zero = unlimited**: a limit of `0` means no cap (admin/premium users).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ── Types ────────────────────────────────────────────────────────────────

/// Per-user resource limits (overridable via admin API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_concurrent_sessions: u32,
    pub max_tokens_per_day: u64,
    pub max_disk_bytes: u64,
    pub max_concurrent_bash: u32,
    pub max_sessions_per_day: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: 5,
            max_tokens_per_day: 2_000_000, // ~$6 at GPT-4 pricing
            max_disk_bytes: 1_073_741_824, // 1 GB
            max_concurrent_bash: 3,
            max_sessions_per_day: 50,
        }
    }
}

/// Daily aggregate usage counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub sessions_created: u32,
    pub tool_calls: u64,
    pub tokens_consumed: u64,
    pub active_sessions: u32,
}

/// Result of a pre-execution limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitCheck {
    /// Proceed — within budget.
    Allowed,
    /// Denied — which limit was hit and a human-readable reason.
    Denied { reason: String },
}

// ── Trait ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ResourceGovernor: Send + Sync + 'static {
    /// Get the effective limits for a user (custom or defaults).
    async fn get_limits(&self, user_id: &str) -> ResourceLimits;

    /// Set custom limits for a user (admin API).
    async fn set_limits(&self, user_id: &str, limits: ResourceLimits);

    /// Get the current daily usage counters for a user.
    async fn get_usage(&self, user_id: &str) -> ResourceUsage;

    /// Check whether a new session can be created for `user_id`.
    async fn check_session_create(&self, user_id: &str) -> LimitCheck;

    /// Record that a session was created (increment daily counter).
    async fn record_session_created(&self, user_id: &str);

    /// Record tool calls executed (fire-and-forget from executor).
    async fn record_tool_calls(&self, user_id: &str, count: u64);

    /// Record tokens consumed.
    async fn record_tokens(&self, user_id: &str, tokens: u64);

    /// Check whether the user's daily token budget allows further LLM calls.
    /// Called before each LLM invocation for mid-session enforcement.
    async fn check_token_budget(&self, user_id: &str) -> LimitCheck {
        let limits = self.get_limits(user_id).await;
        if limits.max_tokens_per_day == 0 {
            return LimitCheck::Allowed;
        }
        let usage = self.get_usage(user_id).await;
        if usage.tokens_consumed >= limits.max_tokens_per_day {
            LimitCheck::Denied {
                reason: format!(
                    "daily token budget exhausted ({}/{})",
                    usage.tokens_consumed, limits.max_tokens_per_day
                ),
            }
        } else {
            LimitCheck::Allowed
        }
    }
}

// ── Database implementation ──────────────────────────────────────────────

use astra_core::SharedPool;

pub struct DatabaseResourceGovernor {
    pool: SharedPool,
}

impl DatabaseResourceGovernor {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Create tables if they don't exist.  Called once at startup from state_builder.
    pub async fn ensure_tables(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS resource_limits (
                user_id       VARCHAR(255) PRIMARY KEY,
                max_concurrent_sessions INT     NOT NULL DEFAULT 5,
                max_tokens_per_day      BIGINT  NOT NULL DEFAULT 2000000,
                max_disk_bytes          BIGINT  NOT NULL DEFAULT 1073741824,
                max_concurrent_bash     INT     NOT NULL DEFAULT 3,
                max_sessions_per_day    INT     NOT NULL DEFAULT 50,
                updated_at              TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(self.pool.get())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS resource_usage (
                user_id           VARCHAR(255) NOT NULL,
                usage_date        DATE         NOT NULL,
                sessions_created  INT          NOT NULL DEFAULT 0,
                tool_calls        BIGINT       NOT NULL DEFAULT 0,
                tokens_consumed   BIGINT       NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, usage_date)
            )
            "#,
        )
        .execute(self.pool.get())
        .await?;

        Ok(())
    }

    /// Count sessions that still "hold" the concurrent cap — open or idle, can be resumed
    /// or listed, but not finished. **`ended` must be excluded**: runs are persisted with
    /// `status = 'ended'` (see `event_ingestion`, `session_reaper`); counting those rows
    /// made the limit hit (e.g. 19/5) as soon as the user had completed past runs.
    async fn count_active_sessions(&self, user_id: &str) -> u32 {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_sessions \
             WHERE user_id = ? \
               AND status NOT IN ('ended', 'closed', 'cancelled')",
        )
        .bind(user_id)
        .fetch_optional(self.pool.get())
        .await
        .unwrap_or(None);
        row.map(|r| r.0 as u32).unwrap_or(0)
    }

    fn today() -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }
}

#[async_trait]
impl ResourceGovernor for DatabaseResourceGovernor {
    async fn get_limits(&self, user_id: &str) -> ResourceLimits {
        let row: Option<(i32, i64, i64, i32, i32)> = sqlx::query_as(
            "SELECT max_concurrent_sessions, max_tokens_per_day, max_disk_bytes, \
                    max_concurrent_bash, max_sessions_per_day \
             FROM resource_limits WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(self.pool.get())
        .await
        .unwrap_or(None);

        match row {
            Some((cs, tp, db, cb, sd)) => ResourceLimits {
                max_concurrent_sessions: cs as u32,
                max_tokens_per_day: tp as u64,
                max_disk_bytes: db as u64,
                max_concurrent_bash: cb as u32,
                max_sessions_per_day: sd as u32,
            },
            None => ResourceLimits::default(),
        }
    }

    async fn set_limits(&self, user_id: &str, limits: ResourceLimits) {
        if let Err(e) = sqlx::query(
            "INSERT INTO resource_limits \
                (user_id, max_concurrent_sessions, max_tokens_per_day, max_disk_bytes, \
                 max_concurrent_bash, max_sessions_per_day) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE \
                max_concurrent_sessions = VALUES(max_concurrent_sessions), \
                max_tokens_per_day = VALUES(max_tokens_per_day), \
                max_disk_bytes = VALUES(max_disk_bytes), \
                max_concurrent_bash = VALUES(max_concurrent_bash), \
                max_sessions_per_day = VALUES(max_sessions_per_day), \
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(limits.max_concurrent_sessions as i32)
        .bind(limits.max_tokens_per_day as i64)
        .bind(limits.max_disk_bytes as i64)
        .bind(limits.max_concurrent_bash as i32)
        .bind(limits.max_sessions_per_day as i32)
        .execute(self.pool.get())
        .await
        {
            tracing::warn!(
                target: "astra_services::resource_governor",
                user_id = %user_id,
                error = %e,
                "failed to persist resource limits"
            );
        }
    }

    async fn get_usage(&self, user_id: &str) -> ResourceUsage {
        let today = Self::today();
        let row: Option<(i32, i64, i64)> = sqlx::query_as(
            "SELECT sessions_created, tool_calls, tokens_consumed \
             FROM resource_usage WHERE user_id = ? AND usage_date = ?",
        )
        .bind(user_id)
        .bind(&today)
        .fetch_optional(self.pool.get())
        .await
        .unwrap_or(None);

        let active = self.count_active_sessions(user_id).await;

        match row {
            Some((sc, tc, tk)) => ResourceUsage {
                sessions_created: sc as u32,
                tool_calls: tc as u64,
                tokens_consumed: tk as u64,
                active_sessions: active,
            },
            None => ResourceUsage {
                active_sessions: active,
                ..Default::default()
            },
        }
    }

    async fn check_session_create(&self, user_id: &str) -> LimitCheck {
        let limits = self.get_limits(user_id).await;
        let usage = self.get_usage(user_id).await;

        if limits.max_concurrent_sessions > 0
            && usage.active_sessions >= limits.max_concurrent_sessions
        {
            return LimitCheck::Denied {
                reason: format!(
                    "concurrent session limit reached ({}/{})",
                    usage.active_sessions, limits.max_concurrent_sessions
                ),
            };
        }

        if limits.max_sessions_per_day > 0 && usage.sessions_created >= limits.max_sessions_per_day
        {
            return LimitCheck::Denied {
                reason: format!(
                    "daily session limit reached ({}/{})",
                    usage.sessions_created, limits.max_sessions_per_day
                ),
            };
        }

        if limits.max_tokens_per_day > 0 && usage.tokens_consumed >= limits.max_tokens_per_day {
            return LimitCheck::Denied {
                reason: format!(
                    "daily token budget exhausted ({}/{})",
                    usage.tokens_consumed, limits.max_tokens_per_day
                ),
            };
        }

        LimitCheck::Allowed
    }

    async fn record_session_created(&self, user_id: &str) {
        let today = Self::today();
        if let Err(e) = sqlx::query(
            "INSERT INTO resource_usage (user_id, usage_date, sessions_created) \
             VALUES (?, ?, 1) \
             ON DUPLICATE KEY UPDATE sessions_created = sessions_created + 1",
        )
        .bind(user_id)
        .bind(&today)
        .execute(self.pool.get())
        .await
        {
            tracing::warn!(
                target: "astra_services::resource_governor",
                user_id = %user_id,
                error = %e,
                "failed to record session creation"
            );
        }
    }

    async fn record_tool_calls(&self, user_id: &str, count: u64) {
        let today = Self::today();
        if let Err(e) = sqlx::query(
            "INSERT INTO resource_usage (user_id, usage_date, tool_calls) \
             VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE tool_calls = tool_calls + VALUES(tool_calls)",
        )
        .bind(user_id)
        .bind(&today)
        .bind(count as i64)
        .execute(self.pool.get())
        .await
        {
            tracing::warn!(
                target: "astra_services::resource_governor",
                user_id = %user_id,
                error = %e,
                "failed to record tool calls"
            );
        }
    }

    async fn record_tokens(&self, user_id: &str, tokens: u64) {
        let today = Self::today();
        if let Err(e) = sqlx::query(
            "INSERT INTO resource_usage (user_id, usage_date, tokens_consumed) \
             VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE tokens_consumed = tokens_consumed + VALUES(tokens_consumed)",
        )
        .bind(user_id)
        .bind(&today)
        .bind(tokens as i64)
        .execute(self.pool.get())
        .await
        {
            tracing::warn!(
                target: "astra_services::resource_governor",
                user_id = %user_id,
                error = %e,
                "failed to record token usage"
            );
        }
    }
}

// ── In-memory implementation (for tests / CLI) ──────────────────────────

pub struct InMemoryResourceGovernor {
    limits: Mutex<HashMap<String, ResourceLimits>>,
    usage: Mutex<HashMap<String, ResourceUsage>>,
}

impl InMemoryResourceGovernor {
    pub fn new() -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryResourceGovernor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ResourceGovernor for InMemoryResourceGovernor {
    async fn get_limits(&self, user_id: &str) -> ResourceLimits {
        self.limits
            .lock()
            .unwrap()
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn set_limits(&self, user_id: &str, limits: ResourceLimits) {
        self.limits
            .lock()
            .unwrap()
            .insert(user_id.to_string(), limits);
    }

    async fn get_usage(&self, user_id: &str) -> ResourceUsage {
        self.usage
            .lock()
            .unwrap()
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn check_session_create(&self, user_id: &str) -> LimitCheck {
        let limits = self.get_limits(user_id).await;
        let usage = self.get_usage(user_id).await;

        if limits.max_concurrent_sessions > 0
            && usage.active_sessions >= limits.max_concurrent_sessions
        {
            return LimitCheck::Denied {
                reason: format!(
                    "concurrent session limit reached ({}/{})",
                    usage.active_sessions, limits.max_concurrent_sessions
                ),
            };
        }
        if limits.max_sessions_per_day > 0 && usage.sessions_created >= limits.max_sessions_per_day
        {
            return LimitCheck::Denied {
                reason: format!(
                    "daily session limit reached ({}/{})",
                    usage.sessions_created, limits.max_sessions_per_day
                ),
            };
        }
        if limits.max_tokens_per_day > 0 && usage.tokens_consumed >= limits.max_tokens_per_day {
            return LimitCheck::Denied {
                reason: format!(
                    "daily token budget exhausted ({}/{})",
                    usage.tokens_consumed, limits.max_tokens_per_day
                ),
            };
        }
        LimitCheck::Allowed
    }

    async fn record_session_created(&self, user_id: &str) {
        let mut map = self.usage.lock().unwrap();
        let usage = map.entry(user_id.to_string()).or_default();
        usage.sessions_created += 1;
        usage.active_sessions += 1;
    }

    async fn record_tool_calls(&self, user_id: &str, count: u64) {
        let mut map = self.usage.lock().unwrap();
        let usage = map.entry(user_id.to_string()).or_default();
        usage.tool_calls += count;
    }

    async fn record_tokens(&self, user_id: &str, tokens: u64) {
        let mut map = self.usage.lock().unwrap();
        let usage = map.entry(user_id.to_string()).or_default();
        usage.tokens_consumed += tokens;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_limits_allow_session_create() {
        let gov = InMemoryResourceGovernor::new();
        assert_eq!(gov.check_session_create("u1").await, LimitCheck::Allowed);
    }

    #[tokio::test]
    async fn concurrent_sessions_denied() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = gov.usage.lock().unwrap();
            map.insert(
                "u1".into(),
                ResourceUsage {
                    active_sessions: 5,
                    ..Default::default()
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { reason } => assert!(reason.contains("concurrent")),
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn daily_session_cap_enforced() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = gov.usage.lock().unwrap();
            map.insert(
                "u1".into(),
                ResourceUsage {
                    sessions_created: 50,
                    ..Default::default()
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { reason } => assert!(reason.contains("daily session")),
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn token_budget_denied_when_exhausted() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = gov.usage.lock().unwrap();
            map.insert(
                "u1".into(),
                ResourceUsage {
                    tokens_consumed: 2_000_000,
                    ..Default::default()
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { reason } => assert!(reason.contains("token budget")),
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn token_budget_allows_within_limit() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = gov.usage.lock().unwrap();
            map.insert(
                "u1".into(),
                ResourceUsage {
                    tokens_consumed: 1_999_999,
                    ..Default::default()
                },
            );
        }
        assert_eq!(gov.check_session_create("u1").await, LimitCheck::Allowed);
    }

    #[tokio::test]
    async fn unlimited_token_budget() {
        let gov = InMemoryResourceGovernor::new();
        gov.set_limits(
            "admin",
            ResourceLimits {
                max_tokens_per_day: 0,
                ..Default::default()
            },
        )
        .await;
        {
            let mut map = gov.usage.lock().unwrap();
            map.insert(
                "admin".into(),
                ResourceUsage {
                    tokens_consumed: 999_999_999,
                    ..Default::default()
                },
            );
        }
        assert_eq!(gov.check_session_create("admin").await, LimitCheck::Allowed);
    }

    #[tokio::test]
    async fn custom_limits_respected() {
        let gov = InMemoryResourceGovernor::new();
        gov.set_limits(
            "vip",
            ResourceLimits {
                max_concurrent_sessions: 100,
                ..Default::default()
            },
        )
        .await;
        let limits = gov.get_limits("vip").await;
        assert_eq!(limits.max_concurrent_sessions, 100);
    }

    #[tokio::test]
    async fn record_accumulates_usage() {
        let gov = InMemoryResourceGovernor::new();
        gov.record_session_created("u1").await;
        gov.record_tool_calls("u1", 5).await;
        gov.record_tokens("u1", 1000).await;
        let u = gov.get_usage("u1").await;
        assert_eq!(u.sessions_created, 1);
        assert_eq!(u.active_sessions, 1);
        assert_eq!(u.tool_calls, 5);
        assert_eq!(u.tokens_consumed, 1000);
    }

    #[tokio::test]
    async fn get_usage_empty_user() {
        let gov = InMemoryResourceGovernor::new();
        let u = gov.get_usage("nobody").await;
        assert_eq!(u.sessions_created, 0);
        assert_eq!(u.active_sessions, 0);
    }

    #[tokio::test]
    async fn user_isolation() {
        let gov = InMemoryResourceGovernor::new();
        gov.record_tool_calls("a", 10).await;
        gov.record_tool_calls("b", 20).await;
        assert_eq!(gov.get_usage("a").await.tool_calls, 10);
        assert_eq!(gov.get_usage("b").await.tool_calls, 20);
    }

    /// audit-D1/D2: DatabaseResourceGovernor must not silently drop DB write
    /// errors. Every `sqlx::query(...).execute(...)` in production code must
    /// be wrapped in error handling, not `let _ =`.
    #[test]
    fn resource_governor_db_writes_are_not_silently_dropped() {
        let source = include_str!("resource_governor.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        let silent_count = prod_code.matches("let _ = sqlx::query").count();
        assert_eq!(
            silent_count, 0,
            "resource governor has {silent_count} silently-dropped DB writes; \
             use `if let Err(e) = ... {{ tracing::warn!(...) }}` instead"
        );
    }

    /// P0-C: A session that starts within budget must be DENIED further
    /// tokens once the daily limit is exceeded mid-session.
    /// This verifies the check_token_budget method exists and works.
    #[tokio::test]
    async fn mid_session_token_enforcement() {
        let gov = InMemoryResourceGovernor::new();
        let user = "u-budget-test";

        // Set a low daily token limit
        gov.set_limits(
            user,
            ResourceLimits {
                max_tokens_per_day: 1000,
                ..Default::default()
            },
        )
        .await;

        // Session starts fine
        assert_eq!(gov.check_session_create(user).await, LimitCheck::Allowed);
        gov.record_session_created(user).await;

        // Consume 800 tokens — still within budget
        gov.record_tokens(user, 800).await;
        assert_eq!(
            gov.check_token_budget(user).await,
            LimitCheck::Allowed,
            "800/1000 tokens should be allowed"
        );

        // Consume 300 more — now over budget (1100 > 1000)
        gov.record_tokens(user, 300).await;
        match gov.check_token_budget(user).await {
            LimitCheck::Denied { reason } => {
                assert!(
                    reason.contains("token"),
                    "denial reason must mention tokens: {reason}"
                );
            }
            LimitCheck::Allowed => {
                panic!("mid-session token check must deny when over budget (1100/1000)")
            }
        }
    }

    /// Zero limit means unlimited — check_token_budget must allow.
    #[tokio::test]
    async fn token_budget_zero_means_unlimited() {
        let gov = InMemoryResourceGovernor::new();
        let user = "u-unlimited";
        gov.set_limits(
            user,
            ResourceLimits {
                max_tokens_per_day: 0, // unlimited
                ..Default::default()
            },
        )
        .await;
        gov.record_tokens(user, 999_999_999).await;
        assert_eq!(
            gov.check_token_budget(user).await,
            LimitCheck::Allowed,
            "zero limit means unlimited"
        );
    }

    /// P0-A: record_tokens must be called after each run so check_token_budget
    /// sees up-to-date usage. Simulates two runs consuming 600 tokens each
    /// against a 1000-token daily cap.
    #[tokio::test]
    async fn token_cap_enforced_across_runs() {
        let gov = InMemoryResourceGovernor::default();
        let user = "user-cap-test";
        gov.set_limits(
            user,
            ResourceLimits {
                max_tokens_per_day: 1000,
                ..Default::default()
            },
        )
        .await;

        // Simulate first run completing and recording tokens
        gov.record_tokens(user, 600).await;
        assert_eq!(
            gov.check_token_budget(user).await,
            LimitCheck::Allowed,
            "600/1000 tokens — should still be allowed"
        );

        // Simulate second run completing and recording tokens
        gov.record_tokens(user, 600).await;
        match gov.check_token_budget(user).await {
            LimitCheck::Denied { reason } => {
                assert!(
                    reason.contains("1200") || reason.contains("1000"),
                    "denial reason must mention token counts, got: {reason}"
                );
            }
            LimitCheck::Allowed => {
                panic!("1200/1000 tokens consumed — must be Denied but got Allowed");
            }
        }
    }

    /// P0-A source guard: run_lifecycle must call record_tokens after the loop.
    #[test]
    fn run_lifecycle_records_tokens_after_loop() {
        let source = include_str!("../../runtime/src/server/run_lifecycle.rs");
        // Find the persist_usage call and verify record_tokens follows it
        let persist_pos = source
            .find("persist_usage")
            .expect("persist_usage must exist");
        let after_persist = &source[persist_pos..];
        let record_pos = after_persist.find("record_tokens");
        assert!(
            record_pos.is_some(),
            "record_tokens must be called after persist_usage in run_lifecycle"
        );
    }
}
