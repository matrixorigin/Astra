//! Centralized runtime limits and tuning parameters.
//!
//! All values have sensible defaults and can be overridden via environment
//! variables, allowing production tuning without recompilation.
//!
//! ```text
//! MO_MAX_TURNS=50              # conversation turns per session
//! MO_MAX_TOOL_ROUNDS=15        # tool execution rounds per turn
//! MO_TURN_TIMEOUT_S=300        # seconds before a turn is force-completed
//! MO_GLOBAL_OUTPUT_LIMIT=80000 # combined tool output bytes
//! MO_TOOL_OUTPUT_LIMIT=30000   # per-tool output bytes
//! MO_MAX_TOOL_RETRIES=3        # transient-error retries per tool
//! MO_RETRY_BASE_MS=500         # base backoff for retries (doubles each)
//! MO_MAX_RETRIEVED=8           # memory/knowledge docs per turn
//! MO_MAX_TURN_INPUT_TOKENS=80000 # max LLM input tokens per turn (0=unlimited)
//! ```

use std::sync::OnceLock;

/// Global runtime limits, loaded once from env on first access.
static LIMITS: OnceLock<RuntimeLimits> = OnceLock::new();

/// Centralized runtime limits.  Read from `MO_*` env vars with defaults.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    /// Maximum conversation turns per session.
    pub max_turns: usize,
    /// Maximum tool execution rounds per turn.
    pub max_tool_rounds: i64,
    /// Per-turn hard timeout in seconds.
    pub turn_timeout_s: f64,
    /// Combined tool output truncation limit (bytes).
    pub global_output_limit: usize,
    /// Per-tool output truncation limit (bytes).
    pub tool_output_limit: usize,
    /// Maximum transient-error retries per tool invocation.
    pub max_tool_retries: usize,
    /// Base backoff delay for tool retries (milliseconds, doubles each attempt).
    pub retry_base_ms: u64,
    /// Maximum memory/knowledge-base documents retrieved per turn.
    pub max_retrieved: usize,
    /// Maximum LLM input tokens per turn before the loop forces a wrap-up.
    /// Prevents runaway context growth that triggers endpoint TPM errors.
    /// 0 = unlimited (legacy default).
    pub max_turn_input_tokens: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_rounds: 15,
            turn_timeout_s: 300.0,
            global_output_limit: 200_000,
            tool_output_limit: 80_000,
            max_tool_retries: 2,
            retry_base_ms: 500,
            max_retrieved: 6,
            max_turn_input_tokens: 80_000,
        }
    }
}

impl RuntimeLimits {
    /// Load limits from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            max_turns: env_parse("MO_MAX_TURNS", d.max_turns),
            max_tool_rounds: env_parse("MO_MAX_TOOL_ROUNDS", d.max_tool_rounds),
            turn_timeout_s: env_parse("MO_TURN_TIMEOUT_S", d.turn_timeout_s),
            global_output_limit: env_parse("MO_GLOBAL_OUTPUT_LIMIT", d.global_output_limit),
            tool_output_limit: env_parse("MO_TOOL_OUTPUT_LIMIT", d.tool_output_limit),
            max_tool_retries: env_parse("MO_MAX_TOOL_RETRIES", d.max_tool_retries),
            retry_base_ms: env_parse("MO_RETRY_BASE_MS", d.retry_base_ms),
            max_retrieved: env_parse("MO_MAX_RETRIEVED", d.max_retrieved),
            max_turn_input_tokens: env_parse("MO_MAX_TURN_INPUT_TOKENS", d.max_turn_input_tokens),
        }
    }

    /// Get the global `RuntimeLimits` singleton (loaded from env on first call).
    pub fn global() -> &'static RuntimeLimits {
        LIMITS.get_or_init(Self::from_env)
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ── Default password constant ───────────────────────────────────────────────

/// Default MatrixOne password used in development mode only.
/// Production deployments MUST set `MATRIXONE_PASSWORD` env var.
pub const DEV_MATRIXONE_PASSWORD: &str = "111";

/// Emit a one-time warning if using the default MatrixOne password.
pub fn warn_default_credentials_once() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    if std::env::var("MATRIXONE_PASSWORD").is_err() {
        WARNED.call_once(|| {
            eprintln!(
                "[config] WARN: using default MatrixOne password. \
                 Set MATRIXONE_PASSWORD env var for production."
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_original_constants() {
        let d = RuntimeLimits::default();
        assert_eq!(d.max_turns, 50);
        assert_eq!(d.max_tool_rounds, 15);
        assert!((d.turn_timeout_s - 300.0).abs() < f64::EPSILON);
        assert_eq!(d.global_output_limit, 200_000);
        assert_eq!(d.tool_output_limit, 80_000);
        assert_eq!(d.max_tool_retries, 2);
        assert_eq!(d.retry_base_ms, 500);
        assert_eq!(d.max_retrieved, 6);
        assert_eq!(d.max_turn_input_tokens, 80_000);
    }

    #[test]
    fn env_override_applies() {
        // We can't safely set env vars in parallel tests, but we can test
        // that env_parse falls back correctly on missing vars.
        let val: usize = env_parse("MO_TEST_NONEXISTENT_LIMIT_XYZ", 42);
        assert_eq!(val, 42);
    }

    #[test]
    fn global_singleton_returns_consistent_values() {
        let a = RuntimeLimits::global();
        let b = RuntimeLimits::global();
        assert_eq!(a.max_turns, b.max_turns);
        assert_eq!(a.max_tool_rounds, b.max_tool_rounds);
    }

    #[test]
    fn dev_password_constant_matches_original() {
        assert_eq!(DEV_MATRIXONE_PASSWORD, "111");
    }
}
