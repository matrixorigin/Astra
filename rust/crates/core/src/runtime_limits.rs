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
//! ASTRA_MAX_TURNS=150             # conversation turns per session
//! ASTRA_PLAN_SUBTASK_MAX_TURNS=0  # per-subtask turn budget (0 = use ASTRA_MAX_TURNS)
//! ASTRA_TURN_TIMEOUT_S=300        # seconds before a turn is force-completed
//! ASTRA_GLOBAL_OUTPUT_LIMIT=200000 # combined tool output bytes
//! ASTRA_TOOL_OUTPUT_LIMIT=80000   # per-tool output bytes
//! ASTRA_MAX_TOOL_RETRIES=2        # transient-error retries per tool
//! ASTRA_RETRY_BASE_MS=500         # base backoff for retries (doubles each)
//! ASTRA_MAX_RETRIEVED=6           # memory/knowledge docs per turn
//! ASTRA_MAX_TURN_INPUT_TOKENS=200000 # max LLM input tokens per turn (0 = use model ceiling only)
//! ```

pub(crate) const DEFAULT_MAX_TURNS: usize = 150;
pub(crate) const DEFAULT_PLAN_SUBTASK_MAX_TURNS: usize = 0;
pub(crate) const DEFAULT_TURN_TIMEOUT_S: u64 = 300;
pub(crate) const DEFAULT_GLOBAL_OUTPUT_LIMIT: usize = 200_000;
pub(crate) const DEFAULT_TOOL_OUTPUT_LIMIT: usize = 80_000;
pub(crate) const DEFAULT_MAX_TOOL_RETRIES: usize = 2;
pub(crate) const DEFAULT_RETRY_BASE_MS: u64 = 500;
pub(crate) const DEFAULT_MAX_RETRIEVED: usize = 6;
pub(crate) const DEFAULT_MAX_TURN_INPUT_TOKENS: u64 = 200_000;

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
    /// When the model has a known context window, derive the model-safe
    /// ceiling from it (roughly 80% — the remaining ~20% covers output
    /// tokens and protocol overhead). The historical 200K default remains
    /// the fallback for unknown models, but it should not clamp known
    /// 1M-window models down to 200K unless the operator explicitly chose
    /// a non-default limit.
    ///
    /// `max_turn_input_tokens = 0` keeps the legacy "unlimited" sentinel:
    /// known models use their model-safe ceiling, unknown models stay
    /// uncapped.
    pub fn effective_max_turn_input_tokens(&self, model: Option<&str>) -> u64 {
        let model_budget = model
            .and_then(context_window_for_model)
            .map(|window| (window as f64 * 0.80) as u64);
        let default_budget = Self::default().max_turn_input_tokens;

        match (model_budget, self.max_turn_input_tokens) {
            (Some(budget), 0) => budget,
            (Some(budget), configured) if configured == default_budget => budget,
            (Some(budget), configured) => budget.min(configured),
            (None, configured) => configured,
        }
    }
}

/// Known context window sizes for common models. Used by the CLI
/// (which doesn't have access to the server-side model registry) to
/// set per-turn token budgets correctly.
///
/// Returns the full context window in tokens. The caller should
/// apply a reserve (e.g., 80% for input, 20% for output).
/// Convenience wrapper around [`context_window_for_model_with_override`].
pub fn context_window_for_model(model: &str) -> Option<u64> {
    context_window_for_model_with_override(model, None)
}

/// Known context window sizes for common models.
///
/// When `config_override` is provided (from `.models.yaml` or the DB),
/// it takes precedence over the hardcoded lookup table. When `None`, falls
/// back to the static lookup table keyed by model name.
///
/// Returns the full context window in tokens. The caller should
/// apply a reserve (e.g., 80% for input, 20% for output).
pub fn context_window_for_model_with_override(
    model: &str,
    config_override: Option<u32>,
) -> Option<u64> {
    // Dynamic override from model config — always wins.
    if let Some(cw) = config_override {
        return Some(cw as u64);
    }

    let lower = model.to_lowercase();
    // OpenAI — GPT-5 family (256K context)
    if lower.contains("gpt-5") {
        return Some(256_000);
    }
    // OpenAI — GPT-4o / GPT-4.1 / GPT-4 Turbo (128K context)
    if lower.contains("gpt-4o") || lower.contains("gpt-4.1") || lower.contains("gpt-4-turbo") {
        return Some(128_000);
    }
    // OpenAI — GPT-4 generic
    if lower.contains("gpt-4") {
        return Some(128_000);
    }
    // OpenAI — GPT-3.5
    if lower.contains("gpt-3.5") {
        return Some(16_000);
    }
    // OpenAI reasoning models
    if has_model_token(&lower, "o1") || has_model_token(&lower, "o3") {
        return Some(200_000);
    }
    // Anthropic 4.6+ generation: 1M context window.
    // The 4.6 generation (Opus 4.6, Sonnet 4.6, Haiku 4.6) advertises a 1M
    // token context. Earlier Claude generations stay at 128K. Match the
    // specific suffix first so legacy members still get the 128K window.
    if lower.contains("opus-4-6")
        || lower.contains("sonnet-4-6")
        || lower.contains("haiku-4-6")
        || lower.contains("opus-4-7")
        || lower.contains("sonnet-4-7")
        || lower.contains("haiku-4-7")
    {
        return Some(1_000_000);
    }
    if lower.contains("claude")
        || lower.contains("opus-4")
        || lower.contains("sonnet-4")
        || lower.contains("haiku-4")
    {
        return Some(128_000);
    }
    // DeepSeek V4 (1M context) — must precede generic deepseek arm
    if lower.contains("deepseek-v4") {
        return Some(1_000_000);
    }
    // DeepSeek V3 / R1 (64K context)
    if lower.contains("deepseek") {
        return Some(64_000);
    }
    // Google — Gemini (1M context)
    if lower.contains("gemini") {
        return Some(1_000_000);
    }
    // Qwen (most have 1M but practical limit is lower)
    if lower.contains("qwen") {
        return Some(128_000);
    }
    // Moonshot / Kimi
    if lower.contains("kimi") || lower.contains("moonshot") {
        return Some(128_000);
    }
    // MiniMax
    if lower.contains("minimax") {
        return Some(200_000);
    }
    None
}

fn has_model_token(model: &str, token: &str) -> bool {
    model
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == token)
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
    fn context_window_recognizes_shared_model_families() {
        assert_eq!(context_window_for_model("gpt-5-turbo"), Some(256_000));
        assert_eq!(context_window_for_model("gpt-3.5-turbo"), Some(16_000));
        assert_eq!(context_window_for_model("o3-mini"), Some(200_000));
        assert_eq!(context_window_for_model("openai/o1-preview"), Some(200_000));
        assert_eq!(context_window_for_model("claude-3.5-sonnet"), Some(128_000));
        assert_eq!(context_window_for_model("kimi-k2"), Some(128_000));
    }

    #[test]
    fn context_window_does_not_misclassify_embedded_o1_or_o3_substrings() {
        assert_eq!(
            context_window_for_model("claude-opus-2025-v01"),
            Some(128_000)
        );
        assert_eq!(context_window_for_model("deepseek-chat-v03"), Some(64_000));
        assert_eq!(context_window_for_model("custom-vision-v03-beta"), None);
    }

    #[test]
    fn effective_max_turn_input_tokens_uses_model_ceiling_for_default_budget() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-v4-pro")),
            800_000
        );
    }

    #[test]
    fn effective_max_turn_input_tokens_never_exceeds_small_model_window() {
        let limits = RuntimeLimits {
            max_turn_input_tokens: 200_000,
            ..Default::default()
        };
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("deepseek-chat")),
            51_200
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
            800_000
        );
        assert_eq!(
            limits.effective_max_turn_input_tokens(Some("unknown-model")),
            0
        );
    }
}
