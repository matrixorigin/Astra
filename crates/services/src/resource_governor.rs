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
//! * **Explicit default budget**: users without an administrator override receive
//!   the product defaults below; an explicit `0` still means no cap.

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

impl ResourceLimits {
    pub const DEFAULT_MAX_CONCURRENT_SESSIONS: u32 = 100;
    pub const DEFAULT_MAX_TOKENS_PER_DAY: u64 = 10_000_000_000;
    pub const DEFAULT_MAX_DISK_BYTES: u64 = 10_737_418_240;
    pub const DEFAULT_MAX_CONCURRENT_BASH: u32 = 100;
    pub const DEFAULT_MAX_SESSIONS_PER_DAY: u32 = 5_000;
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: Self::DEFAULT_MAX_CONCURRENT_SESSIONS,
            max_tokens_per_day: Self::DEFAULT_MAX_TOKENS_PER_DAY,
            max_disk_bytes: Self::DEFAULT_MAX_DISK_BYTES,
            max_concurrent_bash: Self::DEFAULT_MAX_CONCURRENT_BASH,
            max_sessions_per_day: Self::DEFAULT_MAX_SESSIONS_PER_DAY,
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

/// Per-user usage with the date it was last reset.
#[derive(Debug, Clone)]
struct DatedUsage {
    date: chrono::NaiveDate,
    usage: ResourceUsage,
}

/// Machine-readable limit class for quota denials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimitKind {
    ConcurrentSessions,
    DailySessions,
    DailyTokens,
}

impl ResourceLimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConcurrentSessions => "concurrent_sessions",
            Self::DailySessions => "daily_sessions",
            Self::DailyTokens => "daily_tokens",
        }
    }

    pub fn error_code(self) -> &'static str {
        match self {
            Self::ConcurrentSessions => "per_user_concurrent_session_quota",
            Self::DailySessions => "per_user_daily_session_quota",
            Self::DailyTokens => "per_user_daily_token_quota",
        }
    }
}

/// Result of a pre-execution limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitCheck {
    /// Proceed — within budget.
    Allowed,
    /// Denied — which limit was hit and a human-readable reason.
    Denied {
        limit: ResourceLimitKind,
        reason: String,
    },
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

    /// Check whether a new agentic run can start for `user_id`.
    ///
    /// This intentionally does not inspect or mutate `sessions_created`.
    /// A web chat session can have many turns, and each turn starts a durable
    /// run. Counting every run as a newly-created session would make a normal
    /// conversation exhaust the daily session cap.
    async fn check_run_start(&self, user_id: &str) -> LimitCheck {
        let limits = self.get_limits(user_id).await;
        let usage = self.get_usage(user_id).await;

        if limits.max_concurrent_sessions > 0
            && usage.active_sessions >= limits.max_concurrent_sessions
        {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::ConcurrentSessions,
                reason: format!(
                    "concurrent session limit reached ({}/{})",
                    usage.active_sessions, limits.max_concurrent_sessions
                ),
            };
        }

        if limits.max_tokens_per_day > 0 && usage.tokens_consumed >= limits.max_tokens_per_day {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::DailyTokens,
                reason: format!(
                    "daily token budget exhausted ({}/{})",
                    usage.tokens_consumed, limits.max_tokens_per_day
                ),
            };
        }

        LimitCheck::Allowed
    }

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
                limit: ResourceLimitKind::DailyTokens,
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
        let resource_limits_ddl = format!(
            r#"
            CREATE TABLE IF NOT EXISTS resource_limits (
                user_id       VARCHAR(255) PRIMARY KEY,
                max_concurrent_sessions INT     NOT NULL DEFAULT {},
                max_tokens_per_day      BIGINT  NOT NULL DEFAULT {},
                max_disk_bytes          BIGINT  NOT NULL DEFAULT {},
                max_concurrent_bash     INT     NOT NULL DEFAULT {},
                max_sessions_per_day    INT     NOT NULL DEFAULT {},
                updated_at              TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            ResourceLimits::DEFAULT_MAX_CONCURRENT_SESSIONS,
            ResourceLimits::DEFAULT_MAX_TOKENS_PER_DAY,
            ResourceLimits::DEFAULT_MAX_DISK_BYTES,
            ResourceLimits::DEFAULT_MAX_CONCURRENT_BASH,
            ResourceLimits::DEFAULT_MAX_SESSIONS_PER_DAY,
        );
        sqlx::query(&resource_limits_ddl)
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

    /// Count sessions that currently hold execution capacity.
    ///
    /// Web sessions are durable and resumable, so historical/open chat sessions must
    /// not consume the concurrent cap. The cap is enforced on sessions with an
    /// active run only; otherwise a user who has more than five persisted chats
    /// would be unable to start a new turn.
    async fn count_active_sessions(&self, user_id: &str) -> u32 {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COUNT(DISTINCT session_id) FROM agent_runs \
             WHERE user_id = ? \
               AND status IN ('running', 'paused', 'waiting')",
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
                limit: ResourceLimitKind::ConcurrentSessions,
                reason: format!(
                    "concurrent session limit reached ({}/{})",
                    usage.active_sessions, limits.max_concurrent_sessions
                ),
            };
        }

        if limits.max_sessions_per_day > 0 && usage.sessions_created >= limits.max_sessions_per_day
        {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::DailySessions,
                reason: format!(
                    "daily session limit reached ({}/{})",
                    usage.sessions_created, limits.max_sessions_per_day
                ),
            };
        }

        if limits.max_tokens_per_day > 0 && usage.tokens_consumed >= limits.max_tokens_per_day {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::DailyTokens,
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
    usage: Mutex<HashMap<String, DatedUsage>>,
}

impl InMemoryResourceGovernor {
    pub fn new() -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create usage for today, resetting daily counters if the date changed.
    /// `active_sessions` is preserved across resets (it tracks live state, not daily aggregate).
    fn get_or_reset(entry: &mut DatedUsage, today: chrono::NaiveDate) -> &mut ResourceUsage {
        if entry.date != today {
            let active = entry.usage.active_sessions;
            entry.date = today;
            entry.usage = ResourceUsage {
                active_sessions: active,
                ..Default::default()
            };
        }
        &mut entry.usage
    }

    /// Evict stale entries: users with no active sessions whose usage is from a previous day.
    /// Called lazily during reads to bound memory growth in multi-tenant deployments.
    fn evict_stale(map: &mut HashMap<String, DatedUsage>, today: chrono::NaiveDate) {
        map.retain(|_, entry| entry.date == today || entry.usage.active_sessions > 0);
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
            .unwrap_or_else(|e| e.into_inner())
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn set_limits(&self, user_id: &str, limits: ResourceLimits) {
        self.limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user_id.to_string(), limits);
    }

    async fn get_usage(&self, user_id: &str) -> ResourceUsage {
        let today = chrono::Utc::now().date_naive();
        let mut map = astra_core::sync_poison::recover_mutex_lock(&self.usage);
        Self::evict_stale(&mut map, today);
        match map.get_mut(user_id) {
            Some(entry) => Self::get_or_reset(entry, today).clone(),
            None => ResourceUsage::default(),
        }
    }

    async fn check_session_create(&self, user_id: &str) -> LimitCheck {
        let limits = self.get_limits(user_id).await;
        let usage = self.get_usage(user_id).await;

        if limits.max_concurrent_sessions > 0
            && usage.active_sessions >= limits.max_concurrent_sessions
        {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::ConcurrentSessions,
                reason: format!(
                    "concurrent session limit reached ({}/{})",
                    usage.active_sessions, limits.max_concurrent_sessions
                ),
            };
        }
        if limits.max_sessions_per_day > 0 && usage.sessions_created >= limits.max_sessions_per_day
        {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::DailySessions,
                reason: format!(
                    "daily session limit reached ({}/{})",
                    usage.sessions_created, limits.max_sessions_per_day
                ),
            };
        }
        if limits.max_tokens_per_day > 0 && usage.tokens_consumed >= limits.max_tokens_per_day {
            return LimitCheck::Denied {
                limit: ResourceLimitKind::DailyTokens,
                reason: format!(
                    "daily token budget exhausted ({}/{})",
                    usage.tokens_consumed, limits.max_tokens_per_day
                ),
            };
        }
        LimitCheck::Allowed
    }

    async fn record_session_created(&self, user_id: &str) {
        let today = chrono::Utc::now().date_naive();
        let mut map = astra_core::sync_poison::recover_mutex_lock(&self.usage);
        let entry = map
            .entry(user_id.to_string())
            .or_insert_with(|| DatedUsage {
                date: today,
                usage: ResourceUsage::default(),
            });
        let usage = Self::get_or_reset(entry, today);
        usage.sessions_created += 1;
    }

    async fn record_tool_calls(&self, user_id: &str, count: u64) {
        let today = chrono::Utc::now().date_naive();
        let mut map = astra_core::sync_poison::recover_mutex_lock(&self.usage);
        let entry = map
            .entry(user_id.to_string())
            .or_insert_with(|| DatedUsage {
                date: today,
                usage: ResourceUsage::default(),
            });
        Self::get_or_reset(entry, today).tool_calls += count;
    }

    async fn record_tokens(&self, user_id: &str, tokens: u64) {
        let today = chrono::Utc::now().date_naive();
        let mut map = astra_core::sync_poison::recover_mutex_lock(&self.usage);
        let entry = map
            .entry(user_id.to_string())
            .or_insert_with(|| DatedUsage {
                date: today,
                usage: ResourceUsage::default(),
            });
        Self::get_or_reset(entry, today).tokens_consumed += tokens;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_default_limits_match_contract() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_concurrent_sessions, 100);
        assert_eq!(limits.max_tokens_per_day, 10_000_000_000);
        assert_eq!(limits.max_disk_bytes, 10_737_418_240);
        assert_eq!(limits.max_concurrent_bash, 100);
        assert_eq!(limits.max_sessions_per_day, 5_000);
    }

    #[tokio::test]
    async fn default_limits_allow_session_create() {
        let gov = InMemoryResourceGovernor::new();
        assert_eq!(gov.check_session_create("u1").await, LimitCheck::Allowed);
    }

    #[tokio::test]
    async fn concurrent_sessions_denied() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        active_sessions: ResourceLimits::DEFAULT_MAX_CONCURRENT_SESSIONS,
                        ..Default::default()
                    },
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { limit, reason } => {
                assert_eq!(limit, ResourceLimitKind::ConcurrentSessions);
                assert!(reason.contains("concurrent"));
            }
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn daily_session_cap_enforced() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        sessions_created: ResourceLimits::DEFAULT_MAX_SESSIONS_PER_DAY,
                        ..Default::default()
                    },
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { limit, reason } => {
                assert_eq!(limit, ResourceLimitKind::DailySessions);
                assert!(reason.contains("daily session"));
            }
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn configured_token_budget_denies_at_limit() {
        let gov = InMemoryResourceGovernor::new();
        gov.set_limits(
            "u1",
            ResourceLimits {
                max_tokens_per_day: 1_000,
                ..Default::default()
            },
        )
        .await;
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        tokens_consumed: 1_000,
                        ..Default::default()
                    },
                },
            );
        }
        match gov.check_session_create("u1").await {
            LimitCheck::Denied { limit, .. } => {
                assert_eq!(limit, ResourceLimitKind::DailyTokens);
            }
            _ => panic!("expected denied"),
        }
    }

    #[tokio::test]
    async fn configured_token_budget_allows_below_limit() {
        let gov = InMemoryResourceGovernor::new();
        gov.set_limits(
            "u1",
            ResourceLimits {
                max_tokens_per_day: 1_000,
                ..Default::default()
            },
        )
        .await;
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        tokens_consumed: 999,
                        ..Default::default()
                    },
                },
            );
        }
        assert_eq!(gov.check_session_create("u1").await, LimitCheck::Allowed);
    }

    #[tokio::test]
    async fn default_token_budget_is_enforced() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        tokens_consumed: ResourceLimits::DEFAULT_MAX_TOKENS_PER_DAY - 1,
                        ..Default::default()
                    },
                },
            );
        }
        assert_eq!(
            gov.get_limits("u1").await.max_tokens_per_day,
            ResourceLimits::DEFAULT_MAX_TOKENS_PER_DAY
        );
        assert_eq!(gov.check_session_create("u1").await, LimitCheck::Allowed);
        assert_eq!(gov.check_token_budget("u1").await, LimitCheck::Allowed);

        gov.record_tokens("u1", 1).await;
        assert!(matches!(
            gov.check_token_budget("u1").await,
            LimitCheck::Denied {
                limit: ResourceLimitKind::DailyTokens,
                ..
            }
        ));
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
        assert_eq!(u.active_sessions, 0);
        assert_eq!(u.tool_calls, 5);
        assert_eq!(u.tokens_consumed, 1000);
    }

    #[tokio::test]
    async fn run_start_does_not_enforce_daily_session_cap() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        sessions_created: 50,
                        active_sessions: 0,
                        tokens_consumed: 0,
                        ..Default::default()
                    },
                },
            );
        }
        assert_eq!(
            gov.check_run_start("u1").await,
            LimitCheck::Allowed,
            "continuing an existing chat must not consume or enforce daily session quota"
        );
    }

    #[tokio::test]
    async fn run_start_still_enforces_execution_capacity() {
        let gov = InMemoryResourceGovernor::new();
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                "u1".into(),
                DatedUsage {
                    date: chrono::Utc::now().date_naive(),
                    usage: ResourceUsage {
                        active_sessions: ResourceLimits::DEFAULT_MAX_CONCURRENT_SESSIONS,
                        ..Default::default()
                    },
                },
            );
        }
        match gov.check_run_start("u1").await {
            LimitCheck::Denied { limit, reason } => {
                assert_eq!(limit, ResourceLimitKind::ConcurrentSessions);
                assert!(reason.contains("concurrent"));
            }
            _ => panic!("expected denied"),
        }
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
            LimitCheck::Denied { limit, reason } => {
                assert_eq!(limit, ResourceLimitKind::DailyTokens);
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
            LimitCheck::Denied { limit, reason } => {
                assert_eq!(limit, ResourceLimitKind::DailyTokens);
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

    /// P1-A: Daily counters must reset when the date changes.
    /// A user denied yesterday must be allowed today.
    /// active_sessions must survive the reset (it tracks live state).
    #[tokio::test]
    async fn daily_counters_reset_on_date_change() {
        let gov = InMemoryResourceGovernor::new();
        let user = "u-daily-reset";
        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);

        // Simulate arbitrary prior-day usage.
        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            map.insert(
                user.into(),
                DatedUsage {
                    date: yesterday,
                    usage: ResourceUsage {
                        sessions_created: 50,
                        tokens_consumed: 12_345,
                        tool_calls: 999,
                        active_sessions: 2, // live sessions survive reset
                    },
                },
            );
        }

        // Today: daily counters must have reset, so session create is allowed
        assert_eq!(
            gov.check_session_create(user).await,
            LimitCheck::Allowed,
            "daily counters must reset — yesterday's usage must not block today"
        );

        // Verify counters actually reset
        let usage = gov.get_usage(user).await;
        assert_eq!(usage.sessions_created, 0, "sessions_created must reset");
        assert_eq!(usage.tokens_consumed, 0, "tokens_consumed must reset");
        assert_eq!(usage.tool_calls, 0, "tool_calls must reset");
        assert_eq!(
            usage.active_sessions, 2,
            "active_sessions must survive daily reset (live state)"
        );
    }

    /// Stale entries (previous day, no active sessions) must be evicted
    /// to prevent unbounded memory growth in multi-tenant deployments.
    #[tokio::test]
    async fn stale_entries_evicted_on_read() {
        let gov = InMemoryResourceGovernor::new();
        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);

        {
            let mut map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
            // Stale user: yesterday, no active sessions → should be evicted
            map.insert(
                "stale".into(),
                DatedUsage {
                    date: yesterday,
                    usage: ResourceUsage {
                        sessions_created: 10,
                        active_sessions: 0,
                        ..Default::default()
                    },
                },
            );
            // Active user: yesterday but has active sessions → should survive
            map.insert(
                "active".into(),
                DatedUsage {
                    date: yesterday,
                    usage: ResourceUsage {
                        active_sessions: 1,
                        ..Default::default()
                    },
                },
            );
        }

        // Trigger eviction via get_usage
        let _ = gov.get_usage("anyone").await;

        let map = astra_core::sync_poison::recover_mutex_lock(&gov.usage);
        assert!(
            !map.contains_key("stale"),
            "stale entry (yesterday, 0 active) must be evicted"
        );
        assert!(
            map.contains_key("active"),
            "entry with active sessions must survive eviction"
        );
    }
}
