//! Centralized runtime limits and tuning parameters.
//!
//! Infrastructure-level guards that prevent runaway resource consumption.
//! These are NOT policy knobs — policy (stall detection, tool-round hard stops) is
//! handled by `LoopCircuitBreaker` in `astra-turn-core` and `ServerRuntimeConfig`.
//!
//! All values have sensible defaults and can be overridden via environment
//! variables, allowing production tuning without recompilation.
//!
//! ```text
//! ASTRA_MAX_TURNS=300             # conversation turns per session
//! ASTRA_PLAN_SUBTASK_MAX_TURNS=0  # per-subtask turn budget (0 = use ASTRA_MAX_TURNS)
//! ASTRA_TURN_TIMEOUT_S=300        # seconds before a turn is force-completed
//! ASTRA_GLOBAL_OUTPUT_LIMIT=200000 # combined tool output bytes
//! ASTRA_TOOL_OUTPUT_LIMIT=80000   # per-tool output bytes
//! ASTRA_MAX_TOOL_RETRIES=2        # transient-error retries per tool
//! ASTRA_RETRY_BASE_MS=500         # base backoff for retries (doubles each)
//! ASTRA_MAX_RETRIEVED=6           # memory/knowledge docs per turn
//! ASTRA_MAX_TURN_INPUT_TOKENS=200000 # max LLM input tokens per turn (0 = use model ceiling only)
//! ```

pub(crate) const DEFAULT_MAX_TURNS: usize = 300;
pub(crate) const DEFAULT_PLAN_SUBTASK_MAX_TURNS: usize = 0;
pub(crate) const DEFAULT_TURN_TIMEOUT_S: u64 = 300;
pub(crate) const DEFAULT_GLOBAL_OUTPUT_LIMIT: usize = 200_000;
pub(crate) const DEFAULT_TOOL_OUTPUT_LIMIT: usize = 80_000;
pub(crate) const DEFAULT_MAX_TOOL_RETRIES: usize = 2;
pub(crate) const DEFAULT_RETRY_BASE_MS: u64 = 500;
pub(crate) const DEFAULT_MAX_RETRIEVED: usize = 6;
pub(crate) const DEFAULT_MAX_TURN_INPUT_TOKENS: u64 = 200_000;
/// Fraction of a model's full context window made available for prompt input.
///
/// The remaining headroom covers output tokens and provider protocol overhead.
/// Keep this centralized so CLI diagnostics and runtime enforcement do not drift.
pub const MODEL_CONTEXT_INPUT_BUDGET_RATIO: f64 = 0.80;

use std::sync::OnceLock;

/// Global runtime limits, loaded once from env on first access.
static LIMITS: OnceLock<RuntimeLimits> = OnceLock::new();

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
    /// Default: 200_000.
    pub max_turn_input_tokens: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            plan_subtask_max_turns: DEFAULT_PLAN_SUBTASK_MAX_TURNS,
            turn_timeout_s: DEFAULT_TURN_TIMEOUT_S as f64,
            global_output_limit: DEFAULT_GLOBAL_OUTPUT_LIMIT,
            tool_output_limit: DEFAULT_TOOL_OUTPUT_LIMIT,
            max_tool_retries: DEFAULT_MAX_TOOL_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            max_retrieved: DEFAULT_MAX_RETRIEVED,
            max_turn_input_tokens: DEFAULT_MAX_TURN_INPUT_TOKENS,
        }
    }
}

impl RuntimeLimits {
    /// Load limits from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        Self::from_config_with_env(&crate::config::ServerRuntimeConfig::default())
    }

    /// Load limits from a TOML [`ServerRuntimeConfig`] base, then let environment
    /// variables override individual fields. This allows `config.toml` to
    /// set site-specific values while still permitting ad-hoc env tuning.
    pub fn from_config_with_env(cfg: &crate::config::ServerRuntimeConfig) -> Self {
        Self {
            max_turns: env_parse("ASTRA_MAX_TURNS", cfg.max_turns()),
            plan_subtask_max_turns: env_parse(
                "ASTRA_PLAN_SUBTASK_MAX_TURNS",
                cfg.plan_subtask_max_turns(),
            ),
            turn_timeout_s: env_parse("ASTRA_TURN_TIMEOUT_S", cfg.turn_timeout_s() as f64),
            global_output_limit: env_parse("ASTRA_GLOBAL_OUTPUT_LIMIT", cfg.global_output_limit()),
            tool_output_limit: env_parse("ASTRA_TOOL_OUTPUT_LIMIT", cfg.tool_output_limit()),
            max_tool_retries: env_parse("ASTRA_MAX_TOOL_RETRIES", cfg.max_tool_retries()),
            retry_base_ms: env_parse("ASTRA_RETRY_BASE_MS", cfg.retry_base_ms()),
            max_retrieved: env_parse("ASTRA_MAX_RETRIEVED", cfg.max_retrieved()),
            max_turn_input_tokens: env_parse(
                "ASTRA_MAX_TURN_INPUT_TOKENS",
                cfg.max_turn_input_tokens(),
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

    /// Resolve the effective max_turn_input_tokens for a given model.
    ///
    /// When the model registry provides a context window, derive the model-safe
    /// ceiling from it (roughly 80% — the remaining ~20% covers output
    /// tokens and protocol overhead). Without explicit model metadata, keep the
    /// configured runtime limit. The default configured limit is 200K.
    ///
    /// `max_turn_input_tokens = 0` keeps the legacy "unlimited" sentinel:
    /// known models use their model-safe ceiling, unknown models stay
    /// uncapped.
    pub fn effective_max_turn_input_tokens(&self, model: Option<&str>) -> u64 {
        self.effective_max_turn_input_tokens_with_context_window(model, None)
    }

    /// Resolve the effective max_turn_input_tokens for a given model, allowing
    /// the server-side model registry context_window to override static model
    /// name heuristics.
    pub fn effective_max_turn_input_tokens_with_context_window(
        &self,
        model: Option<&str>,
        context_window_override: Option<u32>,
    ) -> u64 {
        let model_budget = context_window_override
            .map(u64::from)
            .or_else(|| model.and_then(context_window_for_model))
            .map(|window| (window as f64 * MODEL_CONTEXT_INPUT_BUDGET_RATIO) as u64);
        let default_budget = Self::default().max_turn_input_tokens;

        match (model_budget, self.max_turn_input_tokens) {
            (Some(budget), 0) => budget,
            (Some(budget), configured) if configured == default_budget => budget,
            (Some(budget), configured) => budget.min(configured),
            (None, configured) => configured,
        }
    }
}

/// Configured context window size for a model.
///
/// This intentionally does not infer limits from model names. The caller must
/// pass a registry/config value via [`context_window_for_model_with_override`]
/// when it has one; otherwise the runtime-level 200K default applies.
/// Convenience wrapper around [`context_window_for_model_with_override`].
pub fn context_window_for_model(model: &str) -> Option<u64> {
    context_window_for_model_with_override(model, None)
}

/// Configured context window size for a model.
///
/// When `config_override` is provided (from `.models.yaml` or the DB), it is
/// the authoritative context window. When it is absent, return `None` so callers
/// fall back to their explicit runtime default instead of guessing by model
/// name.
///
/// Returns the full context window in tokens. The caller should
/// apply a reserve (e.g., 80% for input, 20% for output).
pub fn context_window_for_model_with_override(
    _model: &str,
    config_override: Option<u32>,
) -> Option<u64> {
    config_override.map(u64::from)
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

    #[test]
    fn context_window_override_wins() {
        assert_eq!(
            context_window_for_model_with_override("deepseek-chat", Some(1_000_000)),
            Some(1_000_000)
        );
    }

    #[test]
    fn context_window_does_not_guess_from_model_names() {
        assert_eq!(context_window_for_model("gpt-5-turbo"), None);
        assert_eq!(context_window_for_model("gpt-3.5-turbo"), None);
        assert_eq!(context_window_for_model("o3-mini"), None);
        assert_eq!(
            context_window_for_model("claude-sonnet-4-20250514[1m]"),
            None
        );
        assert_eq!(context_window_for_model("deepseek-v4-pro"), None);
    }

    #[test]
    fn context_window_uses_only_explicit_override() {
        assert_eq!(
            context_window_for_model_with_override("deepseek-v4-pro", Some(1_000_000)),
            Some(1_000_000)
        );
        assert_eq!(context_window_for_model("custom-vision-v03-beta"), None);
    }

    #[test]
    fn effective_max_turn_input_tokens_uses_default_without_context_window() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-v4-pro")),
            200_000
        );
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("claude-sonnet-4-20250514")),
            200_000
        );
    }

    #[test]
    fn effective_max_turn_input_tokens_does_not_guess_small_windows() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-chat")),
            200_000
        );
    }

    #[test]
    fn effective_max_turn_input_tokens_honors_explicit_nondefault_cap() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 150_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-v4-pro")),
            150_000
        );
    }

    #[test]
    fn effective_max_turn_input_tokens_zero_keeps_model_ceiling() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 0,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-v4-pro")),
            0
        );
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("unknown-model")),
            0
        );
    }

    #[test]
    fn effective_max_turn_input_tokens_uses_configured_context_window() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens_with_context_window(
                Some("custom-model"),
                Some(500_000)
            ),
            400_000
        );
        assert_eq!(
            limits.effective_max_turn_input_tokens_with_context_window(
                Some("deepseek-chat"),
                Some(1_000_000)
            ),
            800_000
        );
    }
}
