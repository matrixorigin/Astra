//! Centralized runtime limits and tuning parameters.
//!
//! Infrastructure-level guards that prevent runaway resource consumption.
//! These are NOT policy knobs — policy (stall detection, round budgets) is
//! handled by `LoopCircuitBreaker` in `astra-turn-core` and `RuntimeConfig`.
//!
//! All values have sensible defaults and can be overridden via environment
//! variables, allowing production tuning without recompilation.
//!
//! ```text
//! ASTRA_MAX_TURNS=150             # conversation turns per session
//! ASTRA_PLAN_SUBTASK_MAX_TURNS=0  # per-subtask turn budget (0 = use ASTRA_MAX_TURNS)
//! ASTRA_TURN_TIMEOUT_S=300        # seconds before a turn is force-completed
//! ASTRA_GLOBAL_OUTPUT_LIMIT=200000 # combined tool output bytes
//! ASTRA_TOOL_OUTPUT_LIMIT=80000   # per-tool output bytes
//! ASTRA_MAX_TOOL_RETRIES=2        # transient-error retries per tool
//! ASTRA_RETRY_BASE_MS=500         # base backoff for retries (doubles each)
//! ASTRA_MAX_RETRIEVED=6           # memory/knowledge docs per turn
//! ASTRA_MAX_TURN_INPUT_TOKENS=80000 # max LLM input tokens per turn (0=unlimited)
//! ```

use std::sync::OnceLock;

/// Global runtime limits, loaded once from env on first access.
static LIMITS: OnceLock<RuntimeLimits> = OnceLock::new();

/// Default value for max_tool_rounds — DEPRECATED.
///
/// The per-turn tool round limit is now handled by `LoopCircuitBreaker::absolute_max_rounds`
/// (default 200). This constant is retained only for backward compatibility with
/// code that references it during the transition period.
pub const MAX_TOOL_ROUNDS_DEFAULT: i64 = 200;

/// Centralized runtime limits.  Read from `MO_*` env vars with defaults.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    /// Maximum conversation turns per session.
    pub max_turns: usize,
    /// Per-subtask turn budget for plan execution.
    /// 0 means fall back to `max_turns`.
    pub plan_subtask_max_turns: usize,
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
            max_turns: 150,
            plan_subtask_max_turns: 0,
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
            max_turns: env_parse("ASTRA_MAX_TURNS", d.max_turns),
            plan_subtask_max_turns: env_parse(
                "ASTRA_PLAN_SUBTASK_MAX_TURNS",
                d.plan_subtask_max_turns,
            ),
            turn_timeout_s: env_parse("ASTRA_TURN_TIMEOUT_S", d.turn_timeout_s),
            global_output_limit: env_parse("ASTRA_GLOBAL_OUTPUT_LIMIT", d.global_output_limit),
            tool_output_limit: env_parse("ASTRA_TOOL_OUTPUT_LIMIT", d.tool_output_limit),
            max_tool_retries: env_parse("ASTRA_MAX_TOOL_RETRIES", d.max_tool_retries),
            retry_base_ms: env_parse("ASTRA_RETRY_BASE_MS", d.retry_base_ms),
            max_retrieved: env_parse("ASTRA_MAX_RETRIEVED", d.max_retrieved),
            max_turn_input_tokens: env_parse(
                "ASTRA_MAX_TURN_INPUT_TOKENS",
                d.max_turn_input_tokens,
            ),
        }
    }

    /// Get the global `RuntimeLimits` singleton (loaded from env on first call).
    pub fn global() -> &'static RuntimeLimits {
        LIMITS.get_or_init(Self::from_env)
    }

    /// Effective turn budget for a plan subtask.
    /// Returns `plan_subtask_max_turns` if set (> 0), otherwise `max_turns`.
    pub fn effective_plan_subtask_turns(&self) -> usize {
        if self.plan_subtask_max_turns > 0 {
            self.plan_subtask_max_turns
        } else {
            self.max_turns
        }
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
///
/// Gated behind `dev-defaults` feature (or test builds) so production binaries
/// cannot link this hardcoded fallback.
#[cfg(any(test, feature = "dev-defaults"))]
pub const DEV_MATRIXONE_PASSWORD: &str = "111";

/// Static assertion: ensure const default is updated if changed.
const _: () = assert!(
    MAX_TOOL_ROUNDS_DEFAULT == 200,
    "MAX_TOOL_ROUNDS_DEFAULT should be 200 (update this assertion if intentionally changed)"
);

/// Emit a one-time warning if using the default MatrixOne password.
#[cfg(any(test, feature = "dev-defaults"))]
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
    fn effective_plan_subtask_turns_falls_back_to_max_turns() {
        let limits = RuntimeLimits {
            plan_subtask_max_turns: 0,
            max_turns: 50,
            ..Default::default()
        };
        assert_eq!(limits.effective_plan_subtask_turns(), 50);
    }

    #[test]
    fn effective_plan_subtask_turns_uses_explicit_value() {
        let limits = RuntimeLimits {
            plan_subtask_max_turns: 80,
            max_turns: 50,
            ..Default::default()
        };
        assert_eq!(limits.effective_plan_subtask_turns(), 80);
    }
}
