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
//! ASTRA_MAX_TURNS=300             # optional positive execution-round cap; unset = uncapped
//! ASTRA_PLAN_SUBTASK_MAX_TURNS=100 # optional positive subtask cap; unset = inherit ASTRA_MAX_TURNS
//! ASTRA_TURN_TIMEOUT_S=300        # seconds before a turn is force-completed
//! ASTRA_GLOBAL_OUTPUT_LIMIT=200000 # combined tool output bytes
//! ASTRA_TOOL_OUTPUT_LIMIT=80000   # per-tool output bytes
//! ASTRA_MAX_TOOL_RETRIES=2        # transient-error retries per tool
//! ASTRA_RETRY_BASE_MS=500         # base backoff for retries (doubles each)
//! ASTRA_MAX_RETRIEVED=6           # memory/knowledge docs per turn
//! ASTRA_MAX_TURN_INPUT_TOKENS=200000 # max LLM input tokens per turn (0 = use model ceiling only)
//! ```

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

/// Explicit round constraints. Parsing errors are retained until admission;
/// they must never silently become an uncapped execution policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoundLimits {
    pub max_turns: Option<std::num::NonZeroUsize>,
    pub plan_subtask_max_turns: Option<std::num::NonZeroUsize>,
}

/// Centralized runtime limits.  Read from `MO_*` env vars with defaults.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    /// One authoritative parsing result for optional round constraints.
    pub round_limits: Result<RoundLimits, String>,
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
            round_limits: Ok(RoundLimits::default()),
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
            round_limits: (|| {
                Ok(RoundLimits {
                    max_turns: optional_round_limit("ASTRA_MAX_TURNS", cfg.max_turns)?,
                    plan_subtask_max_turns: optional_round_limit(
                        "ASTRA_PLAN_SUBTASK_MAX_TURNS",
                        cfg.plan_subtask_max_turns,
                    )?,
                })
            })(),
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

    pub fn max_rounds(&self) -> Result<Option<std::num::NonZeroUsize>, String> {
        self.round_limits
            .as_ref()
            .map(|limits| limits.max_turns)
            .map_err(Clone::clone)
    }

    /// Effective turn budget for a plan subtask.
    /// Returns the explicit plan limit, otherwise the optional global limit.
    pub fn effective_plan_subtask_turns(&self) -> Result<Option<std::num::NonZeroUsize>, String> {
        self.round_limits
            .as_ref()
            .map(|limits| limits.plan_subtask_max_turns.or(limits.max_turns))
            .map_err(Clone::clone)
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

    /// Resolve input capacity for an admitted registered model.
    ///
    /// Registered execution must carry its catalog context window. Falling
    /// back to a process-wide token number would silently invent a different
    /// physical model limit for child runs.
    pub fn require_admitted_model_input_tokens(
        &self,
        model: Option<&str>,
        context_window: Option<u32>,
    ) -> Result<u64, String> {
        let context_window = context_window.ok_or_else(|| {
            "admitted model execution requires positive context_window metadata".to_string()
        })?;
        if context_window == 0 {
            return Err(
                "admitted model execution requires positive context_window metadata".to_string(),
            );
        }
        Ok(self.effective_max_turn_input_tokens_with_context_window(model, Some(context_window)))
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

fn optional_round_limit(
    key: &str,
    configured: Option<usize>,
) -> Result<Option<std::num::NonZeroUsize>, String> {
    parse_optional_round_limit(key, std::env::var(key), configured)
}

fn parse_optional_round_limit(
    key: &str,
    environment: Result<String, std::env::VarError>,
    configured: Option<usize>,
) -> Result<Option<std::num::NonZeroUsize>, String> {
    match environment {
        Ok(value) => value
            .parse()
            .map(Some)
            .map_err(|_| format!("{key} must be a positive integer when configured")),
        Err(std::env::VarError::NotPresent) => configured
            .map(|value| {
                std::num::NonZeroUsize::new(value)
                    .ok_or_else(|| format!("{key} configured round limit must be positive"))
            })
            .transpose(),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{key} must be valid UTF-8")),
    }
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
    fn explicit_round_limit_parsing_preserves_absence_and_rejects_invalid_values() {
        use std::env::VarError::NotPresent;
        assert_eq!(
            parse_optional_round_limit("limit", Err(NotPresent), None),
            Ok(None)
        );
        assert_eq!(
            parse_optional_round_limit("limit", Err(NotPresent), Some(50)),
            Ok(std::num::NonZeroUsize::new(50))
        );
        assert_eq!(
            parse_optional_round_limit("limit", Ok("70".into()), Some(50)),
            Ok(std::num::NonZeroUsize::new(70))
        );
        for value in ["0", "-1", "", "invalid-private-value"] {
            let error =
                parse_optional_round_limit("limit", Ok(value.into()), Some(50)).unwrap_err();
            assert_eq!(error, "limit must be a positive integer when configured");
        }
        assert!(parse_optional_round_limit("limit", Err(NotPresent), Some(0)).is_err());
    }

    #[test]
    fn invalid_round_policy_remains_an_error_at_admission() {
        let limits = RuntimeLimits {
            round_limits: Err("ASTRA_MAX_TURNS must be positive".to_string()),
            ..Default::default()
        };
        assert_eq!(
            limits.max_rounds(),
            Err("ASTRA_MAX_TURNS must be positive".to_string())
        );
        assert_eq!(limits.effective_plan_subtask_turns(), limits.max_rounds());
    }

    #[test]
    fn omitted_round_limits_do_not_manufacture_a_task_deadline() {
        let limits = RuntimeLimits::default();
        assert_eq!(limits.max_rounds(), Ok(None));
        assert_eq!(limits.effective_plan_subtask_turns(), Ok(None));
    }

    #[test]
    fn effective_plan_subtask_turns_falls_back_to_max_turns() {
        let limits = RuntimeLimits {
            round_limits: Ok(RoundLimits {
                plan_subtask_max_turns: None,
                max_turns: std::num::NonZeroUsize::new(50),
            }),
            ..Default::default()
        };
        assert_eq!(
            limits.effective_plan_subtask_turns(),
            Ok(std::num::NonZeroUsize::new(50))
        );
    }

    #[test]
    fn effective_plan_subtask_turns_uses_explicit_value() {
        let limits = RuntimeLimits {
            round_limits: Ok(RoundLimits {
                plan_subtask_max_turns: std::num::NonZeroUsize::new(80),
                max_turns: std::num::NonZeroUsize::new(50),
            }),
            ..Default::default()
        };
        assert_eq!(
            limits.effective_plan_subtask_turns(),
            Ok(std::num::NonZeroUsize::new(80))
        );
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

    #[test]
    fn admitted_model_input_tokens_require_catalog_context_and_honor_admin_cap() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 150_000,
            ..Default::default()
        };
        assert_eq!(
            limits
                .require_admitted_model_input_tokens(Some("custom-model"), Some(1_000_000))
                .unwrap(),
            150_000
        );
        assert!(
            limits
                .require_admitted_model_input_tokens(Some("custom-model"), None)
                .is_err()
        );
        assert!(
            limits
                .require_admitted_model_input_tokens(Some("custom-model"), Some(0))
                .is_err()
        );
    }
}
