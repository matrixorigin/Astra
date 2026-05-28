//! Runtime Configuration — parameterized strategies for context management.
//!
//! Replaces hardcoded values with configurable parameters, enabling:
//! - Per-user/per-project customization
//! - A/B testing of different strategies
//! - Auto-tuning based on feedback
//!
//! Configuration hierarchy (later overrides earlier):
//! 1. Built-in defaults
//! 2. ~/.astra/config/runtime.toml (user level)
//! 3. .astra/config/runtime.toml (project level)
//! 4. Environment variables (ASTRA_CONFIG_*)
//! 5. Runtime overrides (via API)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

// ─── Top-Level Configuration ─────────────────────────────────────────────────

/// Complete runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Configuration version for compatibility checking.
    #[serde(default = "default_config_version")]
    pub version: String,

    /// Compression strategy configuration.
    #[serde(default)]
    pub compression: CompressionConfig,

    /// Memory retrieval configuration.
    #[serde(default)]
    pub memory: MemoryConfig,

    /// Tool selection configuration.
    #[serde(default)]
    pub tool_selection: ToolSelectionConfig,

    /// Per-session trace configuration.
    #[serde(default)]
    pub trace: SessionTraceConfig,

    /// Token budget configuration.
    #[serde(default)]
    pub token_budget: TokenBudgetConfig,

    /// Adaptive verification / review strictness.
    #[serde(default)]
    pub verification: VerificationConfig,

    /// Adaptive memory-retrieval pressure.
    #[serde(default)]
    pub memory_pressure: MemoryPressureConfig,

    /// Adaptive context-window / token-burn management.
    #[serde(default)]
    pub context_window: ContextWindowConfig,

    /// Adaptive tuning engine parameters (cooldowns, cycle intervals).
    #[serde(default)]
    pub adaptive_tuning: AdaptiveTuningConfig,

    /// Safety-guard configuration.
    ///
    /// Controls shell-obfuscation guard relaxation for trusted local
    /// environments. Defaults to Strict — never flip this silently.
    #[serde(default)]
    pub safety: SafetyConfig,

    /// Fork-prefix cache inheritance configuration.
    ///
    /// Controls whether child spawns inherit their parent's cacheable
    /// prefix (for prompt-cache reuse) and how fork-cache telemetry
    /// events are emitted. Defaults to disabled — operators must
    /// opt in explicitly.
    #[serde(default)]
    pub fork_prefix: ForkPrefixConfig,

    /// Tool surface configuration — which tools are pinned into the
    /// LLM `tools[]` array vs. exposed through the deferred
    /// system-reminder listing. See [`ToolSurfaceConfig`].
    #[serde(default)]
    pub tool_surface: ToolSurfaceConfig,

    /// Per-turn agentic-loop budget (max tool calls per user message).
    ///
    /// Mirrors the env-driven [`astra_core::RuntimeLimits`] knobs but
    /// lives in `runtime.toml` so operators can edit them via the
    /// `/config` panel without exporting environment variables.
    /// `RuntimeLimits` env values still apply when this section is left
    /// at defaults (the CLI prefers config values > 0 over env).
    #[serde(default)]
    pub runtime_limits: RuntimeLimitsConfig,
}

// ─── Runtime Limits Configuration ────────────────────────────────────────────

/// Per-turn agentic-loop budget knobs editable via `/config`.
///
/// Defaults of 0 mean "fall through to [`astra_core::RuntimeLimits`]"
/// (which itself defaults to 150/0). Setting a positive value here
/// overrides the env-driven default for the CLI without requiring a
/// process restart with new `ASTRA_*` exports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeLimitsConfig {
    /// Max tool calls per user message (regular chat turn).
    /// 0 = inherit from `RuntimeLimits::max_turns` (env / built-in 150).
    #[serde(default)]
    pub max_turns: u32,

    /// Max tool calls per plan subtask. 0 = fall back to `max_turns`.
    #[serde(default)]
    pub plan_subtask_max_turns: u32,
}

impl RuntimeLimitsConfig {
    /// Resolve the effective per-turn tool-call ceiling.
    ///
    /// Config values > 0 override the env-driven [`astra_core::RuntimeLimits`]
    /// singleton. A plan subtask first consults `plan_subtask_max_turns`,
    /// then falls back to `max_turns`, then finally to the env/built-in
    /// `effective_plan_subtask_turns()` behavior.
    pub fn resolve_turn_ceiling(&self, is_plan_subtask: bool) -> usize {
        let env_limits = astra_core::RuntimeLimits::global();
        if is_plan_subtask {
            if self.plan_subtask_max_turns > 0 {
                self.plan_subtask_max_turns as usize
            } else if self.max_turns > 0 {
                self.max_turns as usize
            } else {
                env_limits.effective_plan_subtask_turns()
            }
        } else if self.max_turns > 0 {
            self.max_turns as usize
        } else {
            env_limits.max_turns
        }
    }
}

// ─── Fork-Prefix Configuration ───────────────────────────────────────────────

/// Telemetry sink selection for fork-cache events.
///
/// Serialized as lowercase strings in TOML so config files stay
/// readable (`sink = "stderr"` rather than `sink = "Stderr"`). Adding
/// a new variant is a breaking config change — update the TOML docs
/// and any deployed config files in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ForkCacheSinkKind {
    /// No events emitted. Safe default — zero observable behavior.
    #[default]
    Noop,
    /// Write each event as a JSON line to stderr with `[fork-cache]`
    /// prefix. Intended for local development and `jq`-friendly
    /// pipelines without a full observability backend.
    Stderr,
}

/// Fork-prefix pipeline configuration.
///
/// Captures happen on every parent turn end, spawns with
/// `inherit_prefix` reuse captured prefixes, and telemetry events
/// flow to the configured `sink`.
///
/// Thresholds tune the classifier in `fork_cache_event::evaluate`.
///
/// Invariants:
/// - `miss_floor > 0.0`
/// - `hit_threshold > miss_floor`
/// - `hit_threshold <= 1.0`
///
/// Invalid values silently fall back to classifier defaults at
/// runtime (via `ForkCacheThresholds::validate`); callers that want
/// strict config rejection should `validate()` the loaded config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkPrefixConfig {
    /// Deprecated no-op. Fork capture is now always-on. Retained
    /// for backward-compatible TOML deserialization only.
    #[serde(default)]
    pub enabled: bool,

    /// Telemetry sink to install. `Noop` discards events; `Stderr`
    /// writes JSON lines with `[fork-cache]` prefix.
    #[serde(default)]
    pub sink: ForkCacheSinkKind,

    /// Ratio (observed/expected cache_read_tokens) at or above
    /// which a probe classifies as `Hit`. Default 0.80.
    #[serde(default = "default_fork_hit_threshold")]
    pub hit_threshold: f64,

    /// Ratio below which a probe classifies as `Miss`. Between
    /// `miss_floor` and `hit_threshold` is `PartialDrift`. Default
    /// 0.05 — distinguishes "essentially nothing reused" from
    /// "some reuse happened".
    #[serde(default = "default_fork_miss_floor")]
    pub miss_floor: f64,
}

fn default_fork_hit_threshold() -> f64 {
    0.80
}

fn default_fork_miss_floor() -> f64 {
    0.05
}

impl Default for ForkPrefixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sink: ForkCacheSinkKind::Noop,
            hit_threshold: default_fork_hit_threshold(),
            miss_floor: default_fork_miss_floor(),
        }
    }
}

/// Safety-guard configuration.
///
/// Kept deliberately small — this struct is a contract between config files
/// and the runtime. The `trust_mode` field maps 1:1 to
/// `astra_turn_core::safety_middleware::TrustMode`.
///
/// `trust_mode` is `Option<TrustModeSerde>` so config layering can
/// distinguish three cases:
/// - `None` — layer didn't mention safety; defer to earlier layers / default.
/// - `Some(Strict)` — layer explicitly wants Strict; overrides an earlier
///   `Some(Trusted)` so a project-level config can re-tighten a local opt-in.
/// - `Some(Trusted)` — layer explicitly opts in to relaxed checks.
///
/// Callers that just want the effective mode should use
/// [`SafetyConfig::resolved_trust_mode`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SafetyConfig {
    /// Raw trust-mode field from TOML. Use [`Self::resolved_trust_mode`]
    /// to read the effective value with the default applied.
    ///
    /// - `"strict"` — all rules fire. Safe for any environment.
    /// - `"trusted"` — opt-in developer-local relaxation of high-false-positive
    ///   rules (command substitution). Prompt-injection defenses still apply.
    ///
    /// Unknown values fail to parse (no silent fallback).
    #[serde(default)]
    pub trust_mode: Option<TrustModeSerde>,
}

impl SafetyConfig {
    /// Effective trust mode, applying the Strict default when unset.
    ///
    /// This is what the runtime and CLI should consult — the `Option` is
    /// only load-bearing for config merging (see the merge semantics in
    /// [`RuntimeConfig::merge`]).
    #[must_use]
    pub fn resolved_trust_mode(&self) -> TrustModeSerde {
        self.trust_mode.unwrap_or_default()
    }
}

/// Serializable trust-mode string. Matches the snake-case variants of
/// `astra_turn_core::safety_middleware::TrustMode`.
///
/// `Copy` + `Hash` + `Ord` are free for a unit-variant enum and let this
/// value be used as a map key, passed by value on hot paths, and sorted
/// without a custom impl. No invariants depend on the absence of these.
#[derive(
    Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TrustModeSerde {
    #[default]
    Strict,
    Trusted,
}

fn default_config_version() -> String {
    "1.0".to_string()
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            version: default_config_version(),
            compression: CompressionConfig::default(),
            memory: MemoryConfig::default(),
            tool_selection: ToolSelectionConfig::default(),
            trace: SessionTraceConfig::default(),
            token_budget: TokenBudgetConfig::default(),
            verification: VerificationConfig::default(),
            memory_pressure: MemoryPressureConfig::default(),
            context_window: ContextWindowConfig::default(),
            adaptive_tuning: AdaptiveTuningConfig::default(),
            safety: SafetyConfig::default(),
            fork_prefix: ForkPrefixConfig::default(),
            tool_surface: ToolSurfaceConfig::default(),
            runtime_limits: RuntimeLimitsConfig::default(),
        }
    }
}

// ─── Tool Surface Configuration ──────────────────────────────────────────────

/// User-editable tool surfacing policy.
///
/// The runtime exposes tools to the LLM in two tiers:
///
/// - **Pinned** — schemas live in the request `tools[]` array on every turn.
///   Small set, byte-stable across a session so the Anthropic prompt cache
///   hits. Default = 12 coding-core tools (see runtime `DEFAULT_PINNED`).
/// - **Deferred** — every other tool appears as `name + short_desc` in a
///   system-reminder block. The model calls `tool_search(query="select:X")`
///   to pull a schema into context when it needs X.
///
/// # `pinned_tools` semantics
///
/// **Within a single config file** (e.g. one `runtime.toml`), entries
/// apply additively to the built-in [`DEFAULT_PINNED`](runtime crate) set:
/// - A plain name (e.g. `"github"`) *adds* that tool to the pinned set.
/// - A name prefixed with `-` (e.g. `"-grep"`) *removes* a default from
///   the pinned set (it lands in deferred instead).
/// - Unknown names, whitespace-only, bare `-`, or `--foo` are silently
///   ignored (see `ToolSurface::build`).
///
/// **Across config layers** (user `~/.astra/config/runtime.toml` vs.
/// project `.astra/config/runtime.toml`), the merge is **atomic, not
/// additive**: a project-level `pinned_tools` list fully replaces the
/// user-level one. This is intentional — a project should own its tool
/// surface without silently inheriting a user's personal pins. If you
/// want user-level pins in a project session, copy them into the project
/// file. See `merge()` at the bottom of this file for the precise rule.
///
/// Example `runtime.toml`:
/// ```toml
/// [tool_surface]
/// pinned_tools = ["github", "memory", "-grep"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSurfaceConfig {
    /// Tools to pin into the LLM `tools[]` array. See struct-level
    /// doc for within-file (additive) vs cross-file (atomic replace)
    /// semantics.
    #[serde(default)]
    pub pinned_tools: Vec<String>,
}

// ─── Compression Configuration ───────────────────────────────────────────────

/// Configuration for context compression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    /// Maximum tokens to allocate to conversation history.
    #[serde(default = "default_max_history_tokens")]
    pub max_history_tokens: u32,

    /// Budget pressure threshold to trigger compression (0.0-1.0).
    /// At 0.8, compression triggers when 80% of budget is used.
    #[serde(default = "default_compression_threshold")]
    pub compression_threshold: f64,

    /// Whether to preserve turns containing tool calls.
    #[serde(default = "default_true")]
    pub preserve_tool_calls: bool,

    /// Number of recent turns to always preserve (never compress).
    #[serde(default = "default_preserve_recent_turns")]
    pub preserve_recent_turns: u32,

    /// Maximum length of tool result content before truncation.
    #[serde(default = "default_max_tool_result_length")]
    pub max_tool_result_length: u32,

    /// Strategy preset (overrides individual settings if set).
    #[serde(default)]
    pub strategy: CompressionStrategy,
}

fn default_max_history_tokens() -> u32 {
    40000
}
fn default_compression_threshold() -> f64 {
    0.8
}
fn default_true() -> bool {
    true
}
fn default_preserve_recent_turns() -> u32 {
    3
}
fn default_max_tool_result_length() -> u32 {
    8000
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            max_history_tokens: default_max_history_tokens(),
            compression_threshold: default_compression_threshold(),
            preserve_tool_calls: default_true(),
            preserve_recent_turns: default_preserve_recent_turns(),
            max_tool_result_length: default_max_tool_result_length(),
            strategy: CompressionStrategy::default(),
        }
    }
}

/// Predefined compression strategy presets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionStrategy {
    /// Aggressive compression, minimize token usage.
    Aggressive,
    /// Balanced compression (default).
    #[default]
    Balanced,
    /// Preserve as much context as possible.
    PreserveAll,
    /// Custom (use individual settings).
    Custom,
}

impl CompressionStrategy {
    /// Apply preset values to a config.
    pub fn apply_to(&self, config: &mut CompressionConfig) {
        match self {
            Self::Aggressive => {
                config.max_history_tokens = 20000;
                config.compression_threshold = 0.6;
                config.preserve_recent_turns = 2;
                config.max_tool_result_length = 4000;
            }
            Self::Balanced => {
                config.max_history_tokens = 40000;
                config.compression_threshold = 0.8;
                config.preserve_recent_turns = 3;
                config.max_tool_result_length = 8000;
            }
            Self::PreserveAll => {
                config.max_history_tokens = 80000;
                config.compression_threshold = 0.95;
                config.preserve_recent_turns = 10;
                config.max_tool_result_length = 16000;
            }
            Self::Custom => {
                // Don't override anything
            }
        }
    }
}

// ─── Memory Configuration ────────────────────────────────────────────────────

/// Configuration for memory retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Maximum number of memories to retrieve.
    #[serde(default = "default_retrieval_top_k")]
    pub retrieval_top_k: u32,

    /// Minimum relevance score for a memory to be included (0.0-1.0).
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,

    /// Weight for session-local memories (vs long-term).
    #[serde(default = "default_session_weight")]
    pub session_weight: f64,

    /// Weight for long-term memories.
    #[serde(default = "default_long_term_weight")]
    pub long_term_weight: f64,

    /// Maximum tokens to allocate to memories.
    #[serde(default = "default_max_memory_tokens")]
    pub max_memory_tokens: u32,

    /// Whether to include repository memories.
    #[serde(default = "default_true")]
    pub include_repository_memories: bool,

    /// Strategy preset.
    #[serde(default)]
    pub strategy: MemoryStrategy,
}

fn default_retrieval_top_k() -> u32 {
    5
}
fn default_min_relevance_score() -> f64 {
    0.3
}
fn default_session_weight() -> f64 {
    1.0
}
fn default_long_term_weight() -> f64 {
    0.8
}
fn default_max_memory_tokens() -> u32 {
    4000
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            retrieval_top_k: default_retrieval_top_k(),
            min_relevance_score: default_min_relevance_score(),
            session_weight: default_session_weight(),
            long_term_weight: default_long_term_weight(),
            max_memory_tokens: default_max_memory_tokens(),
            include_repository_memories: default_true(),
            strategy: MemoryStrategy::default(),
        }
    }
}

/// Predefined memory strategy presets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStrategy {
    /// Minimal memory usage.
    Minimal,
    /// Standard memory usage (default).
    #[default]
    Standard,
    /// Comprehensive memory retrieval.
    Comprehensive,
    /// Custom (use individual settings).
    Custom,
}

// ─── Tool Selection Configuration ────────────────────────────────────────────

/// Configuration for tool selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSelectionConfig {
    /// Maximum number of tools to include in the prompt.
    #[serde(default = "default_max_tools")]
    pub max_tools: u32,

    /// Minimum confidence score for a tool to be selected.
    #[serde(default = "default_tool_confidence_threshold")]
    pub confidence_threshold: f64,

    /// Whether to prefer tools used recently in the conversation.
    #[serde(default = "default_true")]
    pub prefer_recent_tools: bool,

    /// Boost factor for recently used tools.
    #[serde(default = "default_recent_tool_boost")]
    pub recent_tool_boost: f64,

    /// Maximum tokens for tool schemas.
    #[serde(default = "default_max_tool_schema_tokens")]
    pub max_tool_schema_tokens: u32,

    /// Deprecated scenario-driven override for the removed tool selector budget.
    /// 0 = no override. Retained so older config files continue to deserialize.
    #[serde(default)]
    pub tool_budget_tokens: u32,

    /// Max times the same (tool, args) can execute across a session.
    /// 0 = use default (2). Prevents infinite loops from ignored dedup hints.
    #[serde(default)]
    pub max_identical_tool_calls: u32,

    /// Max tool calls to execute in a single LLM turn (headless round).
    /// 0 = use default (15). Excess calls are skipped with a budget stub.
    /// Prevents pathological turns where the agent requests 50+ tool calls.
    #[serde(default)]
    pub max_tools_per_turn: u32,

    /// Round budget warning — DEPRECATED, always ignored.
    /// Retained for config file backward compatibility (deserialization won't fail).
    #[serde(default)]
    pub round_budget_warning: u32,

    /// Round budget limit — DEPRECATED, always ignored.
    /// Retained for config file backward compatibility (deserialization won't fail).
    #[serde(default)]
    pub round_budget_limit: u32,

    /// Circuit breaker: consecutive stall rounds (no new patterns, no mutations)
    /// before tripping. 0 = use default (3).
    #[serde(default)]
    pub circuit_breaker_stall_threshold: u32,

    /// Circuit breaker: consecutive identical tool-signature rounds before
    /// tripping. 0 = use default (3).
    #[serde(default)]
    pub circuit_breaker_repetition_threshold: u32,

    /// Circuit breaker: rounds of patience in half-open state after injecting
    /// a correction. 0 = use default (2).
    #[serde(default)]
    pub circuit_breaker_half_open_patience: u32,

    /// Circuit breaker: absolute maximum rounds per turn (infrastructure guard).
    /// 0 = use default (200). This is a pure bug-catcher, not a policy knob.
    #[serde(default)]
    pub circuit_breaker_absolute_max_rounds: u32,

    /// Circuit breaker: consecutive read-only rounds (tools called but no
    /// mutation) before tripping, regardless of signature novelty. Catches
    /// "creative but unproductive" exploration loops. 0 = use default (12).
    #[serde(default)]
    pub circuit_breaker_read_only_stall_threshold: u32,

    /// Circuit breaker: maximum number of introspect (self-check) soft-signals
    /// emitted per turn before the breaker falls back to Continue. Prevents
    /// unbounded self-check prompts on genuinely long read-only sessions.
    ///
    /// - `0` = use default (3).
    /// - Any explicit value ≥ 1 is honored (floor is 1).
    /// - For effectively unbounded behavior, set a very large value
    ///   (e.g. `u32::MAX`) rather than `0` — `0` is reserved for "use default".
    #[serde(default)]
    pub circuit_breaker_max_introspect_emissions: u32,

    /// Mid-loop guard: number of consecutive single-tool rounds tolerated
    /// before the runtime injects a parallel-batching corrective. 0 = use
    /// default (5 — one above the prompt-layer nudge at streak 4 so the
    /// soft→hard cascade is preserved). Lower values intervene more
    /// aggressively; higher values give the model more rope before
    /// correction.
    #[serde(default)]
    pub parallel_batching_force_streak: u32,

    /// Mid-loop guard: count of redundant overlapping reads of the same file
    /// (no intervening edit) tolerated before the runtime injects a
    /// "use existing context" corrective. 0 = use default (4). Tune lower
    /// to intervene sooner on read-loop turns; tune higher to leave models
    /// more rope.
    #[serde(default)]
    pub redundant_reads_midloop_threshold: u32,

    /// Post-mortem eval signal threshold: longest run of consecutive
    /// single-tool rounds required before emitting `SequentialReadChurn`.
    /// 0 = use default (8). Lower values make passive scoring stricter;
    /// higher values make the signal rarer.
    #[serde(default)]
    pub sequential_read_churn_eval_threshold: u32,

    /// Post-mortem eval signal threshold: redundant overlapping reads of the
    /// same file (no intervening mutation) required before emitting
    /// `RedundantOverlappingReads`. 0 = use default (3). Lower values make
    /// passive scoring stricter; higher values make the signal rarer.
    #[serde(default)]
    pub redundant_reads_eval_threshold: u32,

    /// Post-mortem eval signal threshold: grep/rg/find-like calls required
    /// before emitting `SearchFanout`. 0 = use default (8). Lower values make
    /// passive scoring stricter; higher values make the signal rarer.
    #[serde(default)]
    pub search_fanout_eval_threshold: u32,

    /// Post-mortem eval signal threshold: redundant retries of the same heavy
    /// validation command prefix (cargo check/test/build, tsc, npm test, etc.)
    /// required before emitting `RedundantValidationRetries`. 0 = use default
    /// (2). Lower values make passive scoring stricter; higher values make the
    /// signal rarer.
    #[serde(default)]
    pub redundant_validation_retries_eval_threshold: u32,

    /// Mid-loop guard: count of cache-waste tool calls (same tool+args, cached
    /// result) tolerated before the runtime injects a corrective. 0 = use
    /// default (3).
    #[serde(default)]
    pub cache_waste_midloop_threshold: u32,

    /// Mid-loop guard: count of exploration-family churn rounds (same family
    /// dominates consecutive rounds) tolerated before the runtime injects a
    /// corrective. 0 = use default (3).
    #[serde(default)]
    pub exploration_family_churn_midloop_threshold: u32,

    /// Per-model overrides for workflow-guard thresholds.
    ///
    /// Matched against the request's `model` field. The first matching profile
    /// wins; fields left at 0 fall back to the global `ToolSelectionConfig`
    /// defaults. Typical layout:
    ///
    /// ```toml
    /// [[tool_selection.model_profiles]]
    /// model_match = "opus"            # prefix match on model id
    /// max_identical_tool_calls = 4
    ///
    /// [[tool_selection.model_profiles]]
    /// model_match = "haiku"
    /// max_identical_tool_calls = 2
    /// ```
    ///
    /// Built-in defaults are seeded from [`ToolSelectionConfig::builtin_model_profiles`]
    /// when no user profiles match; explicit user entries always take priority.
    #[serde(default)]
    pub model_profiles: Vec<ModelPolicyProfile>,
}

/// Per-model override for workflow-guard thresholds.
///
/// A profile only tunes workflow guards (dedup, turn budget, empty-name stall).
/// Security guards (`shell_obfuscation`, `destructive_sql`) are never affected
/// — those protect against prompt injection and must stay uniform across models.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelPolicyProfile {
    /// Model match pattern. Matched as a case-insensitive substring against
    /// the request's `model` id. `"opus"` matches `"claude-opus-4-7"`,
    /// `"us.anthropic.claude-opus-4-6-v1"`, etc.
    ///
    /// Empty string matches any model (use as a fallback profile).
    #[serde(default)]
    pub model_match: String,

    /// Override for `max_identical_tool_calls`. 0 = inherit from the global
    /// [`ToolSelectionConfig`].
    #[serde(default)]
    pub max_identical_tool_calls: u32,

    /// Override for `max_tools_per_turn`. 0 = inherit.
    #[serde(default)]
    pub max_tools_per_turn: u32,

    /// After this many consecutive cache-hit suppressions on identical args,
    /// the pipeline switches to hard-refusal instead of a soft hint.
    /// 0 = inherit. Replaces the former hardcoded
    /// `REPEATED_CACHE_HIT_SUPPRESSION_THRESHOLD` (was 2).
    #[serde(default)]
    pub repeated_cache_hit_suppression: u32,

    /// Abort a headless round after this many consecutive empty-name tool
    /// calls from the model. 0 = inherit. Replaces the former hardcoded
    /// `MAX_CONSECUTIVE_EMPTY_NAME` (was 3).
    #[serde(default)]
    pub max_consecutive_empty_name: u32,

    /// Override for the mid-loop parallel-batching force streak threshold.
    /// 0 = inherit from the global [`ToolSelectionConfig`].
    #[serde(default)]
    pub parallel_batching_force_streak: u32,
}

/// Resolved per-model workflow-guard policy.
///
/// Returned by [`ToolSelectionConfig::resolve_for_model`]. All fields are
/// concrete (no sentinel zeros) — callers can use them directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveToolPolicy {
    pub max_identical_tool_calls: u32,
    pub max_tools_per_turn: u32,
    /// How many times the same cached (tool, args) may be suppressed with a
    /// soft hint before switching to hard-refusal.
    pub repeated_cache_hit_suppression: u32,
    /// Abort headless round after this many consecutive empty-name calls.
    pub max_consecutive_empty_name: u32,
    /// Mid-loop guard threshold for escalating single-tool streaks into a
    /// parallel-batching corrective.
    pub parallel_batching_force_streak: u32,
}

impl ToolSelectionConfig {
    /// Resolved max identical tool calls (0 → default of 3, floor of 2).
    ///
    /// Default raised from 2 → 3 on 2026-04-27: the prior limit fired on the
    /// common "read → re-check after an edit" flow, which is legitimate rather
    /// than a loop. Per-model profiles can tighten or loosen this further —
    /// see [`ToolSelectionConfig::resolve_for_model`].
    ///
    /// The floor of 2 is symmetric with the per-profile floor in
    /// `apply_profile`. A value of 1 would turn every second identical call
    /// into a dedup hit — almost always a misconfig.
    pub fn effective_max_identical_calls(&self) -> u32 {
        if self.max_identical_tool_calls > 0 {
            self.max_identical_tool_calls.max(2)
        } else {
            3
        }
    }

    /// Resolve workflow-guard thresholds for a given model id.
    ///
    /// Lookup order:
    /// 1. Explicit user profiles in `model_profiles` (first substring match wins)
    /// 2. Built-in profiles from [`Self::builtin_model_profiles`]
    /// 3. Global defaults from `effective_*` methods
    ///
    /// `None` (no model id supplied) → global defaults only.
    pub fn resolve_for_model(&self, model: Option<&str>) -> EffectiveToolPolicy {
        let base = EffectiveToolPolicy {
            max_identical_tool_calls: self.effective_max_identical_calls(),
            max_tools_per_turn: self.effective_max_tools_per_turn(),
            // Defaults raised from 2 → 3 alongside `max_identical_tool_calls`
            // on 2026-04-27; same rationale (read-after-edit verification is
            // legitimate, not a loop).
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            parallel_batching_force_streak: self.effective_parallel_batching_force_streak(),
        };

        let Some(model) = model.map(str::to_ascii_lowercase) else {
            return base;
        };

        let user_hit = self
            .model_profiles
            .iter()
            .find(|p| model_profile_matches(&p.model_match, &model));
        if let Some(profile) = user_hit {
            return apply_profile(base, profile);
        }

        let builtin_hit = Self::builtin_model_profiles()
            .iter()
            .find(|p| model_profile_matches(&p.model_match, &model));
        if let Some(profile) = builtin_hit {
            return apply_profile(base, profile);
        }

        base
    }

    /// Built-in per-model profiles, used when the user has not configured a
    /// matching `model_profiles` entry.
    ///
    /// Keep this list small and defensible. Rule of thumb: stronger models
    /// (less prone to loops) get more rope; weaker/cheaper models stay at
    /// conservative defaults. Security guards are unaffected.
    pub fn builtin_model_profiles() -> &'static [ModelPolicyProfile] {
        // Note: `Default::default()` can't be used in a const context, but
        // the list is small enough that an explicit literal is clearest.
        static PROFILES: std::sync::OnceLock<Vec<ModelPolicyProfile>> = std::sync::OnceLock::new();
        PROFILES.get_or_init(|| {
            vec![
                // Opus 4.x — strongest Anthropic tier, least prone to loops.
                ModelPolicyProfile {
                    model_match: "opus".to_string(),
                    max_identical_tool_calls: 4,
                    max_tools_per_turn: 20,
                    repeated_cache_hit_suppression: 4,
                    max_consecutive_empty_name: 3,
                    parallel_batching_force_streak: 0,
                },
                // Sonnet 4.x — strong mid tier.
                ModelPolicyProfile {
                    model_match: "sonnet-4".to_string(),
                    max_identical_tool_calls: 4,
                    max_tools_per_turn: 18,
                    repeated_cache_hit_suppression: 4,
                    max_consecutive_empty_name: 3,
                    parallel_batching_force_streak: 0,
                },
                // Haiku — fast tier, keep conservative to catch derps early.
                ModelPolicyProfile {
                    model_match: "haiku".to_string(),
                    max_identical_tool_calls: 2,
                    max_tools_per_turn: 12,
                    repeated_cache_hit_suppression: 2,
                    max_consecutive_empty_name: 2,
                    parallel_batching_force_streak: 0,
                },
                // GPT-5 / o-series — treat as strong tier.
                ModelPolicyProfile {
                    model_match: "gpt-5".to_string(),
                    max_identical_tool_calls: 4,
                    max_tools_per_turn: 20,
                    repeated_cache_hit_suppression: 4,
                    max_consecutive_empty_name: 3,
                    parallel_batching_force_streak: 0,
                },
            ]
        })
    }

    /// Resolved max tools per turn (0 → default of 15, floor of 5).
    pub fn effective_max_tools_per_turn(&self) -> u32 {
        if self.max_tools_per_turn > 0 {
            // Floor of 5 prevents pathological starvation from aggressive scenarios.
            self.max_tools_per_turn.max(5)
        } else {
            15
        }
    }

    /// DEPRECATED — always returns a high value so callers that still check
    /// this never trigger budget pressure.
    pub fn effective_round_budget_warning(&self) -> u32 {
        200
    }

    /// DEPRECATED — always returns a high value so callers that still check
    /// this never trigger the old phase1/phase2 logic.
    pub fn effective_round_budget_limit(&self) -> u32 {
        200
    }

    /// Resolved circuit breaker stall threshold (0 → default 3, floor 2).
    pub fn effective_circuit_breaker_stall_threshold(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_stall_threshold, 6, 3)
    }

    /// Resolved circuit breaker repetition threshold (0 → default 3, floor 2).
    pub fn effective_circuit_breaker_repetition_threshold(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_repetition_threshold, 3, 2)
    }

    /// Resolved circuit breaker half-open patience (0 → default 2, floor 1).
    pub fn effective_circuit_breaker_half_open_patience(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_half_open_patience, 2, 1)
    }

    /// Resolved circuit breaker absolute max rounds (0 → default 200, floor 20).
    pub fn effective_circuit_breaker_absolute_max_rounds(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_absolute_max_rounds, 200, 20)
    }

    pub fn effective_circuit_breaker_read_only_stall_threshold(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_read_only_stall_threshold, 12, 4)
    }

    /// Resolved circuit breaker introspect emissions cap (0 → default 3, floor 1).
    /// Use a high explicit value (e.g. 1000) to approximate "unbounded" behavior.
    pub fn effective_circuit_breaker_max_introspect_emissions(&self) -> u32 {
        resolve_threshold(self.circuit_breaker_max_introspect_emissions, 3, 1)
    }

    /// Resolved parallel-batching force streak threshold.
    ///
    /// Default = [`MIN_PARALLEL_BATCHING_FORCE_STREAK`] (currently
    /// `PARALLEL_BATCHING_NUDGE_THRESHOLD + 1`: the nudge fires at streak 4;
    /// the force is one round later so the model gets exactly one chance to
    /// self-correct before we inject a hard `user`-role corrective).
    /// Floor must stay strictly above `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=4
    /// in `astra_runtime::prompts::system`) so the soft→hard cascade is
    /// preserved even when a user explicitly sets a small override; otherwise
    /// the runtime hard corrective fires before the prompt-layer ever nudges.
    /// The mirror-image floor in `apply_profile` MUST agree.
    ///
    /// This is the canonical non-model baseline used by
    /// [`ToolSelectionConfig::resolve_for_model`] when seeding the base policy.
    pub fn effective_parallel_batching_force_streak(&self) -> u32 {
        resolve_parallel_batching_force_streak(self.parallel_batching_force_streak)
    }

    /// Resolved redundant-reads mid-loop corrective threshold (0 → default
    /// of 4). Floor of 2 prevents pathological aggressive intervention; one
    /// re-read is normal noise and we never want to fire on count = 1.
    pub fn effective_redundant_reads_midloop_threshold(&self) -> u32 {
        resolve_threshold(self.redundant_reads_midloop_threshold, 4, 2)
    }

    /// Resolved post-mortem sequential-read-churn eval threshold (0 →
    /// default of 8). Floor of 2 avoids flagging every isolated single-tool
    /// turn when misconfigured.
    pub fn effective_sequential_read_churn_eval_threshold(&self) -> u32 {
        resolve_threshold(self.sequential_read_churn_eval_threshold, 8, 2)
    }

    /// Resolved post-mortem redundant-reads eval threshold (0 → default of
    /// 3). Floor of 2 avoids flagging the first redundant check when
    /// misconfigured.
    pub fn effective_redundant_reads_eval_threshold(&self) -> u32 {
        resolve_threshold(self.redundant_reads_eval_threshold, 3, 2)
    }

    /// Resolved post-mortem search-fanout eval threshold (0 → default of 8).
    /// Floor of 2 avoids pathological misconfiguration.
    pub fn effective_search_fanout_eval_threshold(&self) -> u32 {
        resolve_threshold(self.search_fanout_eval_threshold, 8, 2)
    }

    /// Resolved post-mortem redundant-validation-retries eval threshold
    /// (0 → default of 2). No floor — 1 is meaningful (flag on first retry).
    pub fn effective_redundant_validation_retries_eval_threshold(&self) -> u32 {
        resolve_threshold(self.redundant_validation_retries_eval_threshold, 2, 1)
    }

    /// Resolved mid-loop cache-waste threshold (0 → default of 3). Floor of 2.
    pub fn effective_cache_waste_midloop_threshold(&self) -> u32 {
        resolve_threshold(self.cache_waste_midloop_threshold, 3, 2)
    }

    /// Resolved mid-loop exploration-family churn threshold (0 → default of 3). Floor of 2.
    pub fn effective_exploration_family_churn_midloop_threshold(&self) -> u32 {
        resolve_threshold(self.exploration_family_churn_midloop_threshold, 3, 2)
    }
}

/// Resolve a `0-means-default` config field: returns `default` when `value`
/// is 0, otherwise `value` clamped to at least `floor`.
fn resolve_threshold(value: u32, default: u32, floor: u32) -> u32 {
    if value > 0 { value.max(floor) } else { default }
}

fn resolve_threshold_with_cap(value: u32, default: u32, floor: u32, cap: u32) -> u32 {
    resolve_threshold(value, default, floor).min(cap)
}

/// Minimum admitted value for `parallel_batching_force_streak` (global and
/// per-model). Mirrors `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=4) in the
/// runtime: the hard corrective MUST fire strictly later than the prompt
/// nudge, so the floor is `nudge + 1 = 5`. Both `effective_*` and
/// `apply_profile` use this constant — they MUST stay in lockstep.
pub const MIN_PARALLEL_BATCHING_FORCE_STREAK: u32 = 5;
/// Upper bound for `parallel_batching_force_streak` to keep the hard
/// corrective reachable even under pathological user overrides.
pub const MAX_PARALLEL_BATCHING_FORCE_STREAK: u32 = 50;

fn resolve_parallel_batching_force_streak(value: u32) -> u32 {
    resolve_threshold_with_cap(
        value,
        MIN_PARALLEL_BATCHING_FORCE_STREAK,
        MIN_PARALLEL_BATCHING_FORCE_STREAK,
        MAX_PARALLEL_BATCHING_FORCE_STREAK,
    )
}

/// Minimum pattern length for a non-empty [`ModelPolicyProfile::model_match`].
///
/// Shorter patterns are almost always a misconfig (`"4"` would match any
/// model containing a `4`, `"us"` would match every Bedrock id, etc.).
/// Rejected patterns are silently ignored at resolve time — use
/// [`ToolSelectionConfig::rejected_model_match_patterns`] to surface them
/// (e.g. `astra config show-policy` prints a warning block for each).
const MIN_MODEL_MATCH_LEN: usize = 3;

/// Case-insensitive substring match for [`ModelPolicyProfile::model_match`].
///
/// Rules:
/// - Empty pattern → matches any model (explicit fallback-profile sentinel).
/// - Pattern shorter than [`MIN_MODEL_MATCH_LEN`] (non-empty) → never
///   matches; treated as a misconfig. See the constant's docs for rationale.
/// - Otherwise → case-insensitive substring match.
fn model_profile_matches(pattern: &str, model_lower: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if pattern.chars().count() < MIN_MODEL_MATCH_LEN {
        return false;
    }
    model_lower.contains(&pattern.to_ascii_lowercase())
}

impl ToolSelectionConfig {
    /// Return every `model_match` pattern in `model_profiles` that is too
    /// short to be considered at resolve time (see [`MIN_MODEL_MATCH_LEN`]).
    ///
    /// These patterns match nothing — intended to surface them through
    /// user-facing tooling (e.g. `astra config show-policy`) so the user
    /// can notice the misconfig. The empty-string fallback pattern is
    /// intentionally accepted and not reported.
    pub fn rejected_model_match_patterns(&self) -> Vec<String> {
        self.model_profiles
            .iter()
            .filter(|p| {
                !p.model_match.is_empty() && p.model_match.chars().count() < MIN_MODEL_MATCH_LEN
            })
            .map(|p| p.model_match.clone())
            .collect()
    }
}

/// Apply a profile's non-zero fields over a base policy.
///
/// Each field has a floor to defend against user typos:
/// - `max_identical_tool_calls` floor 2 — a value of 1 would make every
///   second tool call a dedup hit. Matches the haiku built-in's lower bound.
/// - `max_tools_per_turn` floor 5 — prevents pathological starvation.
/// - `repeated_cache_hit_suppression` floor 1 — 0 would effectively disable
///   the guard.
/// - `max_consecutive_empty_name` floor 1 — 0 would abort on the very first
///   empty-name call.
/// - `parallel_batching_force_streak` floor 5
///   ([`MIN_PARALLEL_BATCHING_FORCE_STREAK`]) — must stay strictly above the
///   prompt-layer nudge threshold so the soft→hard cascade is preserved even
///   when a user configures a too-small per-model override.
fn apply_profile(base: EffectiveToolPolicy, profile: &ModelPolicyProfile) -> EffectiveToolPolicy {
    EffectiveToolPolicy {
        max_identical_tool_calls: if profile.max_identical_tool_calls > 0 {
            profile.max_identical_tool_calls.max(2)
        } else {
            base.max_identical_tool_calls
        },
        max_tools_per_turn: if profile.max_tools_per_turn > 0 {
            profile.max_tools_per_turn.max(5)
        } else {
            base.max_tools_per_turn
        },
        repeated_cache_hit_suppression: if profile.repeated_cache_hit_suppression > 0 {
            profile.repeated_cache_hit_suppression.max(1)
        } else {
            base.repeated_cache_hit_suppression
        },
        max_consecutive_empty_name: if profile.max_consecutive_empty_name > 0 {
            profile.max_consecutive_empty_name.max(1)
        } else {
            base.max_consecutive_empty_name
        },
        parallel_batching_force_streak: if profile.parallel_batching_force_streak > 0 {
            resolve_parallel_batching_force_streak(profile.parallel_batching_force_streak)
        } else {
            base.parallel_batching_force_streak
        },
    }
}

fn default_max_tools() -> u32 {
    30
}
fn default_tool_confidence_threshold() -> f64 {
    0.3
}
fn default_recent_tool_boost() -> f64 {
    0.15
}
fn default_max_tool_schema_tokens() -> u32 {
    15000
}

impl Default for ToolSelectionConfig {
    fn default() -> Self {
        Self {
            max_tools: default_max_tools(),
            confidence_threshold: default_tool_confidence_threshold(),
            prefer_recent_tools: default_true(),
            recent_tool_boost: default_recent_tool_boost(),
            max_tool_schema_tokens: default_max_tool_schema_tokens(),
            tool_budget_tokens: 0,
            max_identical_tool_calls: 0,
            max_tools_per_turn: 0,
            round_budget_warning: 0,
            round_budget_limit: 0,
            circuit_breaker_stall_threshold: 0,
            circuit_breaker_repetition_threshold: 0,
            circuit_breaker_half_open_patience: 0,
            circuit_breaker_absolute_max_rounds: 0,
            circuit_breaker_read_only_stall_threshold: 0,
            circuit_breaker_max_introspect_emissions: 0,
            parallel_batching_force_streak: 0,
            redundant_reads_midloop_threshold: 0,
            sequential_read_churn_eval_threshold: 0,
            redundant_reads_eval_threshold: 0,
            search_fanout_eval_threshold: 0,
            redundant_validation_retries_eval_threshold: 0,
            cache_waste_midloop_threshold: 0,
            exploration_family_churn_midloop_threshold: 0,
            model_profiles: Vec::new(),
        }
    }
}

// ─── Session Trace Configuration ──────────────────────────────────────────────

/// Predefined trace profiles for per-session trace configuration.
///
/// - `Production`: minimal overhead — only `Error` + `Warn` events, core categories.
/// - `Dev`: maximum introspection — `Trace` level, all categories.
/// - `Custom`: fully user-controlled via the other fields on [`SessionTraceConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceProfile {
    Production,
    Dev,
    Custom,
}

impl Default for TraceProfile {
    fn default() -> Self {
        Self::Production
    }
}

/// Trace event categories that can be toggled on/off per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceCategory {
    /// Tool call lifecycle (start, complete, failures).
    ToolCalls,
    /// Full LLM request/response payloads.
    LlmExchanges,
    /// Context assembly and compression decisions.
    ContextAssembly,
    /// Decision explanations (tool selection rationale).
    DecisionExplain,
    /// Phase transitions (Perceive → Plan → Execute → Evaluate → Reflect).
    PhaseTransition,
    /// Budget tracking (set, update, expansion).
    Budget,
    /// Reflection generation and adaptation.
    Reflection,
    /// Verification and review events.
    Verification,
    /// LLM thinking/reasoning content blocks.
    Thinking,
    /// Memory retrieval queries and results.
    MemoryRetrieval,
    /// Skill loading, execution, and teardown lifecycle.
    SkillExecution,
    /// System prompt assembly and injection decisions.
    PromptAssembly,
    /// Safety guard evaluations and rulings.
    GuardEvaluation,
    /// Meta-category: enables all categories.
    All,
}

impl TraceCategory {
    /// Every individual category except `All`.
    pub fn individual_categories() -> &'static [TraceCategory] {
        &[
            TraceCategory::ToolCalls,
            TraceCategory::LlmExchanges,
            TraceCategory::ContextAssembly,
            TraceCategory::DecisionExplain,
            TraceCategory::PhaseTransition,
            TraceCategory::Budget,
            TraceCategory::Reflection,
            TraceCategory::Verification,
            TraceCategory::Thinking,
            TraceCategory::MemoryRetrieval,
            TraceCategory::SkillExecution,
            TraceCategory::PromptAssembly,
            TraceCategory::GuardEvaluation,
        ]
    }
}

/// Where trace events are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSink {
    /// Persist to session journal (JSONL).
    Journal,
    /// Emit to stderr via `tracing` subscriber.
    Stderr,
    /// Export via OpenTelemetry (requires `otel` feature).
    Otlp,
}

/// Per-session trace configuration.
///
/// Each session can set its own trace profile, level, category filter, and sink selection.
///
/// # Resolution order (highest priority first)
/// 1. CLI flags (`--trace-profile`, `--trace-level`, `--trace-cat`)
/// 2. Session metadata / persisted overrides (if session exists)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTraceConfig {
    /// Which pre-built profile to use. [`TraceProfile::Custom`] means the other
    /// fields drive behavior directly.
    #[serde(default)]
    pub profile: TraceProfile,

    /// Minimum severity level for event emission. Events with a lower
    /// default level are silently dropped.
    ///
    /// Serialized as a lowercase string: `"error"`, `"warn"`, `"info"`,
    /// `"debug"`, `"trace"`.
    #[serde(default = "default_trace_level")]
    pub min_level: TraceLevelSerde,

    /// Which event categories to emit. `All` means every category.
    #[serde(default = "default_trace_categories")]
    pub enabled_categories: Vec<TraceCategory>,

    /// Where to deliver trace events. Defaults to `[Journal]`.
    #[serde(default = "default_trace_sinks")]
    pub sinks: Vec<TraceSink>,

    /// Sampling rate (0.0–1.0). 1.0 = emit all non-filtered events;
    /// 0.1 = emit ~10%. Useful for high-volume Trace-level sessions.
    #[serde(default = "default_sampling_rate")]
    pub sampling_rate: f64,
}

/// String-based [`TraceLevel`] for TOML/JSON serialization.
///
/// This duplicates `astra_pipeline::TraceLevel` as a serde-friendly enum so
/// `astra-config` stays decoupled from `astra-pipeline`. Convert with `From`/`Into`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceLevelSerde {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Default for TraceLevelSerde {
    fn default() -> Self {
        Self::Info
    }
}

fn default_trace_level() -> TraceLevelSerde {
    TraceLevelSerde::Info
}

fn default_trace_categories() -> Vec<TraceCategory> {
    vec![TraceCategory::ToolCalls, TraceCategory::PhaseTransition, TraceCategory::Budget]
}

fn default_trace_sinks() -> Vec<TraceSink> {
    vec![TraceSink::Journal]
}

fn default_sampling_rate() -> f64 {
    1.0
}

impl Default for SessionTraceConfig {
    fn default() -> Self {
        Self {
            profile: TraceProfile::default(),
            min_level: default_trace_level(),
            enabled_categories: default_trace_categories(),
            sinks: default_trace_sinks(),
            sampling_rate: default_sampling_rate(),
        }
    }
}

/// Per-session trace config singleton. Set once at session startup via
/// [`SessionTraceConfig::set_current`], read by [`SessionTraceConfig::current`].
static SESSION_TRACE_CONFIG: OnceLock<SessionTraceConfig> = OnceLock::new();

impl SessionTraceConfig {
    /// Store the trace config for the current session. Call once at session
    /// startup before any pipeline modules are created.
    pub fn set_current(config: SessionTraceConfig) {
        let _ = SESSION_TRACE_CONFIG.set(config);
    }

    /// Retrieve the current session's trace config. Returns the stored config
    /// if set, otherwise the default (production-like).
    ///
    /// # Panics
    /// Never — falls back to default on unset.
    #[must_use]
    pub fn current() -> Self {
        SESSION_TRACE_CONFIG.get().cloned().unwrap_or_default()
    }

    /// Apply a preset profile, overwriting relevant fields.
    pub fn apply_profile(mut self, profile: TraceProfile) -> Self {
        match profile {
            TraceProfile::Production => {
                self.min_level = TraceLevelSerde::Warn;
                self.enabled_categories = vec![TraceCategory::ToolCalls, TraceCategory::PhaseTransition];
                self.sinks = vec![TraceSink::Journal];
                self.sampling_rate = 1.0;
            }
            TraceProfile::Dev => {
                self.min_level = TraceLevelSerde::Trace;
                self.enabled_categories = vec![TraceCategory::All];
                self.sinks = vec![TraceSink::Journal, TraceSink::Stderr];
                self.sampling_rate = 1.0;
            }
            TraceProfile::Custom => {
                // keep existing values; caller is responsible for setting them
            }
        }
        self.profile = profile;
        self
    }

    /// Check whether a given category should be emitted.
    pub fn category_enabled(&self, cat: TraceCategory) -> bool {
        if cat == TraceCategory::All {
            return true;
        }
        self.enabled_categories.contains(&TraceCategory::All) || self.enabled_categories.contains(&cat)
    }

    /// Build from CLI flags (`--trace-profile`, `--trace-level`, `--trace-cat`).
    pub fn from_cli(
        profile: Option<&str>,
        level: Option<&str>,
        cats: Option<&str>,
    ) -> Self {
        let mut config = Self::default();
        if let Some(p) = profile {
            config = config.apply_profile(match p {
                "production" => TraceProfile::Production,
                "dev" => TraceProfile::Dev,
                _ => TraceProfile::Custom,
            });
        }
        if let Some(l) = level {
            config.min_level = match l {
                "error" => TraceLevelSerde::Error,
                "warn" => TraceLevelSerde::Warn,
                "info" => TraceLevelSerde::Info,
                "debug" => TraceLevelSerde::Debug,
                "trace" => TraceLevelSerde::Trace,
                _ => TraceLevelSerde::Info,
            };
        }
        if let Some(c) = cats {
            let lower = c.to_lowercase();
            config.enabled_categories = if lower == "all" {
                vec![TraceCategory::All]
            } else {
                lower.split(',').filter_map(|s| match s.trim() {
                    "tool_calls" => Some(TraceCategory::ToolCalls),
                    "llm_exchanges" => Some(TraceCategory::LlmExchanges),
                    "context_assembly" => Some(TraceCategory::ContextAssembly),
                    "decision_explain" => Some(TraceCategory::DecisionExplain),
                    "phase_transition" => Some(TraceCategory::PhaseTransition),
                    "budget" => Some(TraceCategory::Budget),
                    "reflection" => Some(TraceCategory::Reflection),
                    "verification" => Some(TraceCategory::Verification),
                    "thinking" => Some(TraceCategory::Thinking),
                    "memory_retrieval" => Some(TraceCategory::MemoryRetrieval),
                    "skill_execution" => Some(TraceCategory::SkillExecution),
                    "prompt_assembly" => Some(TraceCategory::PromptAssembly),
                    "guard_evaluation" => Some(TraceCategory::GuardEvaluation),
                    _ => None,
                }).collect()
            };
        }
        config
    }
}

// ─── Token Budget Configuration ──────────────────────────────────────────────

/// Configuration for token budget allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetConfig {
    /// Maximum prompt tokens (0 = model default).
    #[serde(default)]
    pub max_prompt_tokens: u32,

    /// Maximum tokens per turn input.
    #[serde(default = "default_max_turn_input_tokens")]
    pub max_turn_input_tokens: u32,

    /// Reserve tokens for system prompt.
    #[serde(default = "default_system_prompt_reserve")]
    pub system_prompt_reserve: u32,

    /// Reserve tokens for tools.
    #[serde(default = "default_tools_reserve")]
    pub tools_reserve: u32,
}

fn default_max_turn_input_tokens() -> u32 {
    200_000
}
fn default_system_prompt_reserve() -> u32 {
    4000
}
fn default_tools_reserve() -> u32 {
    15000
}

impl Default for TokenBudgetConfig {
    fn default() -> Self {
        Self {
            max_prompt_tokens: 0,
            max_turn_input_tokens: default_max_turn_input_tokens(),
            system_prompt_reserve: default_system_prompt_reserve(),
            tools_reserve: default_tools_reserve(),
        }
    }
}

// ─── Verification Configuration ──────────────────────────────────────────────

/// Configuration for adaptive verification / review strictness.
///
/// When adaptive is enabled, the runtime raises or lowers review strictness
/// based on user corrections, drift, and failure patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationConfig {
    /// Whether adaptive strictness adjustment is active.
    #[serde(default = "default_true")]
    pub adaptive: bool,

    /// Current strictness level (0.0 = lenient, 1.0 = maximum).
    #[serde(default = "default_verification_strictness")]
    pub strictness: f64,

    /// Minimum strictness (clamped).
    #[serde(default = "default_verification_min")]
    pub min_strictness: f64,

    /// Maximum strictness (clamped).
    #[serde(default = "default_verification_max")]
    pub max_strictness: f64,

    /// Whether corrections should automatically raise strictness.
    #[serde(default = "default_true")]
    pub increase_on_correction: bool,

    /// Whether detected focus-drift should raise strictness.
    #[serde(default)]
    pub increase_on_drift: bool,
}

fn default_verification_strictness() -> f64 {
    0.5
}
fn default_verification_min() -> f64 {
    0.2
}
fn default_verification_max() -> f64 {
    0.9
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            adaptive: true,
            strictness: default_verification_strictness(),
            min_strictness: default_verification_min(),
            max_strictness: default_verification_max(),
            increase_on_correction: true,
            increase_on_drift: false,
        }
    }
}

// ─── Memory Pressure Configuration ──────────────────────────────────────────

/// Configuration for adaptive memory-retrieval pressure.
///
/// When adaptive is enabled, retrieval top-k and history preservation
/// expand or contract based on tool churn, focus drift, and corrections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPressureConfig {
    /// Whether adaptive memory pressure is active.
    #[serde(default = "default_true")]
    pub adaptive: bool,

    /// Minimum retrieval top-k (adaptive floor).
    #[serde(default = "default_retrieval_min")]
    pub retrieval_min: u32,

    /// Maximum retrieval top-k (adaptive ceiling).
    #[serde(default = "default_retrieval_max")]
    pub retrieval_max: u32,

    /// Expand memory retrieval on tool churn (repeated failures).
    #[serde(default = "default_true")]
    pub expand_on_churn: bool,

    /// Expand memory retrieval on detected focus drift.
    #[serde(default = "default_true")]
    pub expand_on_drift: bool,

    /// Expand memory retrieval on user corrections.
    #[serde(default)]
    pub expand_on_correction: bool,
}

fn default_retrieval_min() -> u32 {
    3
}
fn default_retrieval_max() -> u32 {
    15
}

impl Default for MemoryPressureConfig {
    fn default() -> Self {
        Self {
            adaptive: true,
            retrieval_min: default_retrieval_min(),
            retrieval_max: default_retrieval_max(),
            expand_on_churn: true,
            expand_on_drift: true,
            expand_on_correction: false,
        }
    }
}

// ─── Context-Window Configuration ───────────────────────────────────────────

/// Configuration for adaptive token-budget and compression management.
///
/// When adaptive is enabled, the runtime adjusts token budgets per-turn
/// based on actual burn rate, compression frequency, and error patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowConfig {
    /// Whether adaptive token budgets are active.
    #[serde(default = "default_true")]
    pub adaptive: bool,

    /// Whether the compression threshold adjusts automatically.
    #[serde(default = "default_true")]
    pub dynamic_compression: bool,

    /// Minimum compression threshold (adaptive floor).
    #[serde(default = "default_compression_threshold_min")]
    pub compression_threshold_min: f64,

    /// Maximum compression threshold (adaptive ceiling).
    #[serde(default = "default_compression_threshold_max")]
    pub compression_threshold_max: f64,

    /// Fraction of remaining budget to allocate per remaining turn.
    #[serde(default = "default_remaining_turn_factor")]
    pub remaining_turn_factor: f64,

    /// Tokens reserved for error recovery retries.
    #[serde(default = "default_error_recovery_reserve")]
    pub error_recovery_reserve: u32,

    /// Enables the "at 85% usage, lower `max_turn_input_tokens` by ~10%"
    /// path in `agentic_adaptive_tuning`. Default: OFF.
    ///
    /// Why off by default: the logic is a self-defeating shrink spiral. At
    /// high pressure it LOWERS the ceiling the next turn must fit under,
    /// which raises the probability of another high-pressure event, which
    /// lowers the ceiling again. Session 0e37eb46 hit 36968 tokens via this
    /// path and the model gave up with a progress summary. The compaction
    /// pipeline is the right tool for pressure; the budget should stay put.
    ///
    /// Leave reachable via config for environments that explicitly want
    /// quota-style protection over per-turn headroom.
    #[serde(default)]
    pub adaptive_budget_reduction: bool,
}

fn default_compression_threshold_min() -> f64 {
    0.5
}
fn default_compression_threshold_max() -> f64 {
    0.95
}
fn default_remaining_turn_factor() -> f64 {
    0.33
}
fn default_error_recovery_reserve() -> u32 {
    10_000
}

impl Default for ContextWindowConfig {
    fn default() -> Self {
        Self {
            adaptive: true,
            dynamic_compression: true,
            compression_threshold_min: default_compression_threshold_min(),
            compression_threshold_max: default_compression_threshold_max(),
            remaining_turn_factor: default_remaining_turn_factor(),
            error_recovery_reserve: default_error_recovery_reserve(),
            adaptive_budget_reduction: false,
        }
    }
}

// ─── Adaptive Tuning Configuration ──────────────────────────────────────────

/// Parameters controlling the adaptive tuning engine's timing and dampening.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveTuningConfig {
    /// Minimum turns between scenario changes (anti-flap).
    #[serde(default = "default_scenario_cooldown_turns")]
    pub scenario_cooldown_turns: u32,

    /// Minimum turns between token-budget direction reversals (anti-oscillation).
    #[serde(default = "default_budget_cooldown_turns")]
    pub budget_cooldown_turns: u32,

    /// Number of completed turns between tuning cycle evaluations.
    #[serde(default = "default_tuning_cycle_interval")]
    pub tuning_cycle_interval: u32,
}

fn default_scenario_cooldown_turns() -> u32 {
    5
}
fn default_budget_cooldown_turns() -> u32 {
    3
}
fn default_tuning_cycle_interval() -> u32 {
    5
}

impl Default for AdaptiveTuningConfig {
    fn default() -> Self {
        Self {
            scenario_cooldown_turns: default_scenario_cooldown_turns(),
            budget_cooldown_turns: default_budget_cooldown_turns(),
            tuning_cycle_interval: default_tuning_cycle_interval(),
        }
    }
}

fn merge_if_non_default<T: PartialEq>(slot: &mut T, incoming: T, default: T) {
    if incoming != default {
        *slot = incoming;
    }
}

// ─── CLI `--settings` overlay ────────────────────────────────────────────────

/// Process-wide overlay installed by the CLI front door from `--settings`.
/// `RuntimeConfig::load()` reads this after env overrides and merges it with
/// non-default-wins semantics, so a `--settings` override beats both env and
/// on-disk config for one invocation. `None` = no overlay, normal load.
///
/// Stored as the already-parsed `RuntimeConfig` rather than raw JSON so the
/// CLI boundary is the only place that can fail on malformed input — every
/// `load()` call thereafter is infallible.


static CLI_OVERLAY: std::sync::OnceLock<std::sync::RwLock<Option<RuntimeConfig>>> =
    std::sync::OnceLock::new();

fn cli_overlay_cell() -> &'static std::sync::RwLock<Option<RuntimeConfig>> {
    CLI_OVERLAY.get_or_init(|| std::sync::RwLock::new(None))
}

/// Install a process-wide config overlay. Subsequent `RuntimeConfig::load()`
/// calls merge `overlay` on top of the resolved config. Pass `None` to clear.
///
/// Intended for CLI `--settings`: call once at startup after arg parsing.
/// Safe to call multiple times; the last call wins.
pub fn set_cli_overlay(overlay: Option<RuntimeConfig>) {
    if let Ok(mut slot) = cli_overlay_cell().write() {
        *slot = overlay;
    }
}

fn cli_overlay_snapshot() -> Option<RuntimeConfig> {
    cli_overlay_cell().read().ok().and_then(|s| s.clone())
}

// ─── Configuration Loading ───────────────────────────────────────────────────

impl RuntimeConfig {
    /// Load configuration from default paths.
    ///
    /// Loads in order (later overrides earlier):
    /// Return a process-wide cached `RuntimeConfig`.
    ///
    /// Use this for hot-path reads (per turn / per tool call). `load()`
    /// does sync filesystem I/O and TOML parsing — cheap once, wasteful
    /// repeated. Config is effectively static for the process lifetime;
    /// a restart picks up file edits.
    ///
    /// Use `load()` only at session/process boundary or in tests that
    /// deliberately want a fresh parse.
    pub fn cached() -> &'static Self {
        static CACHED: std::sync::OnceLock<RuntimeConfig> = std::sync::OnceLock::new();
        CACHED.get_or_init(Self::load)
    }

    /// 1. Built-in defaults
    /// 2. ~/.astra/config/runtime.toml
    /// 3. .astra/config/runtime.toml
    /// 4. Environment variables
    /// 5. CLI `--settings` overlay (see [`set_cli_overlay`])
    pub fn load() -> Self {
        let mut config = Self::default();

        // User-level config
        if let Some(home) = dirs::home_dir() {
            let user_config = home.join(".astra/config/runtime.toml");
            if let Ok(content) = std::fs::read_to_string(&user_config) {
                match toml::from_str::<RuntimeConfig>(&content) {
                    Ok(user) => config = config.merge(user),
                    Err(err) => {
                        // Malformed TOML silently falling back to defaults
                        // was the #1 footgun in the P1 review — now logged.
                        tracing::warn!(
                            path = %user_config.display(),
                            error = %err,
                            "invalid runtime.toml at user config path; falling back to defaults"
                        );
                    }
                }
            }
        }

        // Project-level config
        let project_config = PathBuf::from(".astra/config/runtime.toml");
        if let Ok(content) = std::fs::read_to_string(&project_config) {
            match toml::from_str::<RuntimeConfig>(&content) {
                Ok(project) => config = config.merge(project),
                Err(err) => {
                    tracing::warn!(
                        path = %project_config.display(),
                        error = %err,
                        "invalid runtime.toml at project config path; falling back to defaults"
                    );
                }
            }
        }

        // Environment overrides
        config.apply_env_overrides();

        // CLI --settings overlay (process-wide, highest precedence).
        // Applied after env so an operator who wants to undo an env-set
        // knob for one invocation can do so without unsetting the env.
        if let Some(overlay) = cli_overlay_snapshot() {
            config = config.merge(overlay);
        }

        // Apply strategy presets
        if config.compression.strategy != CompressionStrategy::Custom {
            let strategy = config.compression.strategy.clone();
            strategy.apply_to(&mut config.compression);
        }

        // Propagate the resolved trace config into the per-session singleton
        // so that SessionTraceConfig::current() reflects all merged layers
        // (defaults + user config + project config + env + CLI overlay).
        SessionTraceConfig::set_current(config.trace.clone());

        config
    }

    /// Load the resolved config and simultaneously record it in a
    /// content-addressed version store. Returns `(config, version_id)`.
    ///
    /// This is the "front door" every session should walk through at
    /// startup. The returned id is the single small value that flows
    /// into checkpoints, journals, and (later) cloud mirrors as the
    /// canonical reference to "what config this session ran under".
    ///
    /// The store is taken by reference so callers can choose between
    /// the default `~/.astra/config/versions/` `LocalFileStore` and a
    /// tempdir store (tests, ephemeral CLI modes).
    pub fn load_with_version(
        store: &dyn crate::config_versions::ConfigVersionStore,
        meta: crate::config_versions::PutMetadata,
    ) -> Result<(Self, crate::config_versions::VersionId), crate::config_versions::StoreError> {
        let config = Self::load();
        let id = store.put(&config, meta)?;
        Ok((config, id))
    }

    /// Merge another config into this one (other takes precedence).
    pub fn merge(mut self, other: RuntimeConfig) -> Self {
        let RuntimeConfig {
            version,
            compression,
            memory,
            tool_selection,
            trace,
            token_budget,
            verification,
            memory_pressure,
            context_window,
            adaptive_tuning,
            safety,
            fork_prefix,
            tool_surface,
            runtime_limits,
        } = other;

        merge_if_non_default(&mut self.version, version, default_config_version());

        let CompressionConfig {
            max_history_tokens,
            compression_threshold,
            preserve_tool_calls,
            preserve_recent_turns,
            max_tool_result_length,
            strategy,
        } = compression;
        merge_if_non_default(
            &mut self.compression.max_history_tokens,
            max_history_tokens,
            default_max_history_tokens(),
        );
        merge_if_non_default(
            &mut self.compression.compression_threshold,
            compression_threshold,
            default_compression_threshold(),
        );
        merge_if_non_default(
            &mut self.compression.preserve_tool_calls,
            preserve_tool_calls,
            default_true(),
        );
        merge_if_non_default(
            &mut self.compression.preserve_recent_turns,
            preserve_recent_turns,
            default_preserve_recent_turns(),
        );
        merge_if_non_default(
            &mut self.compression.max_tool_result_length,
            max_tool_result_length,
            default_max_tool_result_length(),
        );
        merge_if_non_default(
            &mut self.compression.strategy,
            strategy,
            CompressionStrategy::default(),
        );

        let MemoryConfig {
            retrieval_top_k,
            min_relevance_score,
            session_weight,
            long_term_weight,
            max_memory_tokens,
            include_repository_memories,
            strategy,
        } = memory;
        merge_if_non_default(
            &mut self.memory.retrieval_top_k,
            retrieval_top_k,
            default_retrieval_top_k(),
        );
        merge_if_non_default(
            &mut self.memory.min_relevance_score,
            min_relevance_score,
            default_min_relevance_score(),
        );
        merge_if_non_default(
            &mut self.memory.session_weight,
            session_weight,
            default_session_weight(),
        );
        merge_if_non_default(
            &mut self.memory.long_term_weight,
            long_term_weight,
            default_long_term_weight(),
        );
        merge_if_non_default(
            &mut self.memory.max_memory_tokens,
            max_memory_tokens,
            default_max_memory_tokens(),
        );
        merge_if_non_default(
            &mut self.memory.include_repository_memories,
            include_repository_memories,
            default_true(),
        );
        merge_if_non_default(
            &mut self.memory.strategy,
            strategy,
            MemoryStrategy::default(),
        );

        let ToolSelectionConfig {
            max_tools,
            confidence_threshold,
            prefer_recent_tools,
            recent_tool_boost,
            max_tool_schema_tokens,
            tool_budget_tokens,
            max_identical_tool_calls,
            max_tools_per_turn,
            round_budget_warning,
            round_budget_limit,
            circuit_breaker_stall_threshold,
            circuit_breaker_repetition_threshold,
            circuit_breaker_half_open_patience,
            circuit_breaker_absolute_max_rounds,
            circuit_breaker_read_only_stall_threshold,
            circuit_breaker_max_introspect_emissions,
            parallel_batching_force_streak,
            redundant_reads_midloop_threshold,
            sequential_read_churn_eval_threshold,
            redundant_reads_eval_threshold,
            search_fanout_eval_threshold,
            redundant_validation_retries_eval_threshold,
            cache_waste_midloop_threshold,
            exploration_family_churn_midloop_threshold,
            model_profiles,
        } = tool_selection;
        merge_if_non_default(
            &mut self.tool_selection.max_tools,
            max_tools,
            default_max_tools(),
        );
        merge_if_non_default(
            &mut self.tool_selection.confidence_threshold,
            confidence_threshold,
            default_tool_confidence_threshold(),
        );
        merge_if_non_default(
            &mut self.tool_selection.prefer_recent_tools,
            prefer_recent_tools,
            default_true(),
        );
        merge_if_non_default(
            &mut self.tool_selection.recent_tool_boost,
            recent_tool_boost,
            default_recent_tool_boost(),
        );
        merge_if_non_default(
            &mut self.tool_selection.max_tool_schema_tokens,
            max_tool_schema_tokens,
            default_max_tool_schema_tokens(),
        );
        merge_if_non_default(
            &mut self.tool_selection.tool_budget_tokens,
            tool_budget_tokens,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.max_identical_tool_calls,
            max_identical_tool_calls,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.max_tools_per_turn,
            max_tools_per_turn,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.round_budget_warning,
            round_budget_warning,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.round_budget_limit,
            round_budget_limit,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.circuit_breaker_stall_threshold,
            circuit_breaker_stall_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.circuit_breaker_repetition_threshold,
            circuit_breaker_repetition_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.circuit_breaker_half_open_patience,
            circuit_breaker_half_open_patience,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.circuit_breaker_absolute_max_rounds,
            circuit_breaker_absolute_max_rounds,
            0,
        );
        merge_if_non_default(
            &mut self
                .tool_selection
                .circuit_breaker_read_only_stall_threshold,
            circuit_breaker_read_only_stall_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.circuit_breaker_max_introspect_emissions,
            circuit_breaker_max_introspect_emissions,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.parallel_batching_force_streak,
            parallel_batching_force_streak,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.redundant_reads_midloop_threshold,
            redundant_reads_midloop_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.sequential_read_churn_eval_threshold,
            sequential_read_churn_eval_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.redundant_reads_eval_threshold,
            redundant_reads_eval_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.search_fanout_eval_threshold,
            search_fanout_eval_threshold,
            0,
        );
        merge_if_non_default(
            &mut self
                .tool_selection
                .redundant_validation_retries_eval_threshold,
            redundant_validation_retries_eval_threshold,
            0,
        );
        merge_if_non_default(
            &mut self.tool_selection.cache_waste_midloop_threshold,
            cache_waste_midloop_threshold,
            0,
        );
        merge_if_non_default(
            &mut self
                .tool_selection
                .exploration_family_churn_midloop_threshold,
            exploration_family_churn_midloop_threshold,
            0,
        );
        // model_profiles: non-empty override replaces; empty preserves existing.
        // Merging by model_match would be ambiguous when patterns overlap.
        if !model_profiles.is_empty() {
            self.tool_selection.model_profiles = model_profiles;
        }

        let SessionTraceConfig {
            profile,
            min_level,
            enabled_categories,
            sinks,
            sampling_rate,
        } = trace;
        merge_if_non_default(
            &mut self.trace.profile,
            profile,
            TraceProfile::default(),
        );
        merge_if_non_default(
            &mut self.trace.min_level,
            min_level,
            TraceLevelSerde::default(),
        );
        if !enabled_categories.is_empty() {
            self.trace.enabled_categories = enabled_categories;
        }
        if !sinks.is_empty() {
            self.trace.sinks = sinks;
        }
        merge_if_non_default(
            &mut self.trace.sampling_rate,
            sampling_rate,
            1.0,
        );

        let TokenBudgetConfig {
            max_prompt_tokens,
            max_turn_input_tokens,
            system_prompt_reserve,
            tools_reserve,
        } = token_budget;
        merge_if_non_default(
            &mut self.token_budget.max_prompt_tokens,
            max_prompt_tokens,
            0,
        );
        merge_if_non_default(
            &mut self.token_budget.max_turn_input_tokens,
            max_turn_input_tokens,
            default_max_turn_input_tokens(),
        );
        merge_if_non_default(
            &mut self.token_budget.system_prompt_reserve,
            system_prompt_reserve,
            default_system_prompt_reserve(),
        );
        merge_if_non_default(
            &mut self.token_budget.tools_reserve,
            tools_reserve,
            default_tools_reserve(),
        );

        let VerificationConfig {
            adaptive,
            strictness,
            min_strictness,
            max_strictness,
            increase_on_correction,
            increase_on_drift,
        } = verification;
        merge_if_non_default(&mut self.verification.adaptive, adaptive, default_true());
        merge_if_non_default(
            &mut self.verification.strictness,
            strictness,
            default_verification_strictness(),
        );
        merge_if_non_default(
            &mut self.verification.min_strictness,
            min_strictness,
            default_verification_min(),
        );
        merge_if_non_default(
            &mut self.verification.max_strictness,
            max_strictness,
            default_verification_max(),
        );
        merge_if_non_default(
            &mut self.verification.increase_on_correction,
            increase_on_correction,
            default_true(),
        );
        merge_if_non_default(
            &mut self.verification.increase_on_drift,
            increase_on_drift,
            false,
        );

        let MemoryPressureConfig {
            adaptive,
            retrieval_min,
            retrieval_max,
            expand_on_churn,
            expand_on_drift,
            expand_on_correction,
        } = memory_pressure;
        merge_if_non_default(&mut self.memory_pressure.adaptive, adaptive, default_true());
        merge_if_non_default(
            &mut self.memory_pressure.retrieval_min,
            retrieval_min,
            default_retrieval_min(),
        );
        merge_if_non_default(
            &mut self.memory_pressure.retrieval_max,
            retrieval_max,
            default_retrieval_max(),
        );
        merge_if_non_default(
            &mut self.memory_pressure.expand_on_churn,
            expand_on_churn,
            default_true(),
        );
        merge_if_non_default(
            &mut self.memory_pressure.expand_on_drift,
            expand_on_drift,
            default_true(),
        );
        merge_if_non_default(
            &mut self.memory_pressure.expand_on_correction,
            expand_on_correction,
            false,
        );

        let ContextWindowConfig {
            adaptive,
            dynamic_compression,
            compression_threshold_min,
            compression_threshold_max,
            remaining_turn_factor,
            error_recovery_reserve,
            adaptive_budget_reduction,
        } = context_window;
        merge_if_non_default(&mut self.context_window.adaptive, adaptive, default_true());
        merge_if_non_default(
            &mut self.context_window.dynamic_compression,
            dynamic_compression,
            default_true(),
        );
        merge_if_non_default(
            &mut self.context_window.compression_threshold_min,
            compression_threshold_min,
            default_compression_threshold_min(),
        );
        merge_if_non_default(
            &mut self.context_window.compression_threshold_max,
            compression_threshold_max,
            default_compression_threshold_max(),
        );
        merge_if_non_default(
            &mut self.context_window.remaining_turn_factor,
            remaining_turn_factor,
            default_remaining_turn_factor(),
        );
        merge_if_non_default(
            &mut self.context_window.error_recovery_reserve,
            error_recovery_reserve,
            default_error_recovery_reserve(),
        );
        merge_if_non_default(
            &mut self.context_window.adaptive_budget_reduction,
            adaptive_budget_reduction,
            false,
        );

        // ── Adaptive Tuning ──
        let AdaptiveTuningConfig {
            scenario_cooldown_turns,
            budget_cooldown_turns,
            tuning_cycle_interval,
        } = adaptive_tuning;
        merge_if_non_default(
            &mut self.adaptive_tuning.scenario_cooldown_turns,
            scenario_cooldown_turns,
            default_scenario_cooldown_turns(),
        );
        merge_if_non_default(
            &mut self.adaptive_tuning.budget_cooldown_turns,
            budget_cooldown_turns,
            default_budget_cooldown_turns(),
        );
        merge_if_non_default(
            &mut self.adaptive_tuning.tuning_cycle_interval,
            tuning_cycle_interval,
            default_tuning_cycle_interval(),
        );

        // SafetyConfig: last layer with an explicit trust_mode wins.
        // Unset (None) preserves the earlier layer's value. This makes the
        // merge symmetric — a project config can both opt *in* to Trusted
        // AND opt *back out* to Strict on top of a Trusted user config.
        if safety.trust_mode.is_some() {
            self.safety = safety;
        }

        // ForkPrefixConfig: whole-struct replacement when `other`
        // differs from default. Simple enough to treat atomically —
        // sub-field merging would just add complexity without
        // meaningful use cases (you either want fork-prefix on with
        // a specific sink / thresholds, or off).
        if fork_prefix != ForkPrefixConfig::default() {
            self.fork_prefix = fork_prefix;
        }

        // ToolSurfaceConfig: whole-struct replacement when `other` is
        // non-empty. `pinned_tools` is a user-expressed override list —
        // additive merge would let a project config silently inherit a
        // user's pins, which is surprising. Treat it atomically: if the
        // project (or env) file sets it, it wins outright.
        if !tool_surface.pinned_tools.is_empty() {
            self.tool_surface = tool_surface;
        }

        let RuntimeLimitsConfig {
            max_turns,
            plan_subtask_max_turns,
        } = runtime_limits;
        merge_if_non_default(&mut self.runtime_limits.max_turns, max_turns, 0);
        merge_if_non_default(
            &mut self.runtime_limits.plan_subtask_max_turns,
            plan_subtask_max_turns,
            0,
        );

        self
    }

    /// Apply environment variable overrides.
    fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("ASTRA_MAX_HISTORY_TOKENS")
            && let Ok(n) = val.parse()
        {
            self.compression.max_history_tokens = n;
        }
        if let Ok(val) = std::env::var("ASTRA_COMPRESSION_THRESHOLD")
            && let Ok(n) = val.parse()
        {
            self.compression.compression_threshold = n;
        }
        if let Ok(val) = std::env::var("ASTRA_RETRIEVAL_TOP_K")
            && let Ok(n) = val.parse()
        {
            self.memory.retrieval_top_k = n;
        }
        if let Ok(val) = std::env::var("ASTRA_MAX_TURN_INPUT_TOKENS")
            && let Ok(n) = val.parse()
        {
            self.token_budget.max_turn_input_tokens = n;
        }
        if let Ok(val) = std::env::var("ASTRA_CAPTURE_TRACES") {
            if val == "1" || val.to_lowercase() == "true" {
                self.trace.enabled_categories = vec![TraceCategory::All];
            }
        }
        if let Ok(val) = std::env::var("ASTRA_CAPTURE_FULL_LLM") {
            if val == "1" || val.to_lowercase() == "true" {
                if !self.trace.enabled_categories.contains(&TraceCategory::LlmExchanges) {
                    self.trace.enabled_categories.push(TraceCategory::LlmExchanges);
                }
            }
        }
        // ASTRA_FORK_INHERIT_PREFIX env var is ignored — fork capture
        // is now always-on. The `enabled` field is a deprecated no-op.
    }

    /// Get configuration as TOML string.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RuntimeConfig::default();
        assert_eq!(config.compression.max_history_tokens, 40000);
        assert!((config.compression.compression_threshold - 0.8).abs() < 0.001);
        assert_eq!(config.memory.retrieval_top_k, 5);
        assert!(!config.trace.category_enabled(TraceCategory::LlmExchanges));
    }

    #[test]
    fn test_effective_max_identical_calls() {
        let mut config = ToolSelectionConfig::default();
        // Default raised from 2 → 3 on 2026-04-27 (see
        // `effective_max_identical_calls` doc).
        assert_eq!(config.effective_max_identical_calls(), 3);

        config.max_identical_tool_calls = 5;
        assert_eq!(config.effective_max_identical_calls(), 5);

        // Floor applied: 1 would make every second identical call a dedup
        // hit (see `apply_profile` for the matching per-model floor).
        config.max_identical_tool_calls = 1;
        assert_eq!(
            config.effective_max_identical_calls(),
            2,
            "global max_identical_tool_calls = 1 must clamp to floor 2, \
             mirroring the per-model floor in `apply_profile`"
        );
    }

    /// The symmetry regression — this asserts the global and per-profile
    /// floors behave the same when both set `max_identical_tool_calls = 1`.
    /// Before the fix, `apply_profile` clamped to 2 but the global path
    /// returned 1 verbatim.
    #[test]
    fn global_and_profile_floors_for_max_identical_calls_agree() {
        let cfg = ToolSelectionConfig {
            max_identical_tool_calls: 1,
            ..Default::default()
        };
        let global = cfg.effective_max_identical_calls();

        let mut cfg2 = ToolSelectionConfig::default();
        cfg2.model_profiles.push(ModelPolicyProfile {
            model_match: "custom".to_string(),
            max_identical_tool_calls: 1,
            ..Default::default()
        });
        let via_profile = cfg2
            .resolve_for_model(Some("custom-model"))
            .max_identical_tool_calls;

        assert_eq!(
            global, via_profile,
            "floor must be symmetric between global and per-profile paths"
        );
    }

    #[test]
    fn test_effective_max_tools_per_turn() {
        let mut config = ToolSelectionConfig::default();
        assert_eq!(config.effective_max_tools_per_turn(), 15);

        config.max_tools_per_turn = 10;
        assert_eq!(config.effective_max_tools_per_turn(), 10);

        // Floor of 5 prevents pathological starvation
        config.max_tools_per_turn = 1;
        assert_eq!(config.effective_max_tools_per_turn(), 5);

        config.max_tools_per_turn = 5;
        assert_eq!(config.effective_max_tools_per_turn(), 5);
    }

    #[test]
    fn test_compression_strategy_presets() {
        let mut config = CompressionConfig::default();

        CompressionStrategy::Aggressive.apply_to(&mut config);
        assert_eq!(config.max_history_tokens, 20000);
        assert!((config.compression_threshold - 0.6).abs() < 0.001);

        CompressionStrategy::PreserveAll.apply_to(&mut config);
        assert_eq!(config.max_history_tokens, 80000);
        assert!((config.compression_threshold - 0.95).abs() < 0.001);
    }

    #[test]
    fn test_config_serialization() {
        let config = RuntimeConfig::default();
        let toml = config.to_toml().unwrap();
        assert!(toml.contains("max_history_tokens"));
        assert!(toml.contains("retrieval_top_k"));
    }

    #[test]
    fn test_env_override() {
        unsafe {
            std::env::set_var("ASTRA_MAX_HISTORY_TOKENS", "50000");
        }
        let mut config = RuntimeConfig::default();
        config.apply_env_overrides();
        assert_eq!(config.compression.max_history_tokens, 50000);
        unsafe {
            std::env::remove_var("ASTRA_MAX_HISTORY_TOKENS");
        }
    }

    #[test]
    fn test_verification_config_defaults() {
        let config = VerificationConfig::default();
        assert!(config.adaptive);
        assert!((config.strictness - 0.5).abs() < 0.001);
        assert!((config.min_strictness - 0.2).abs() < 0.001);
        assert!((config.max_strictness - 0.9).abs() < 0.001);
        assert!(config.increase_on_correction);
        assert!(!config.increase_on_drift);
    }

    #[test]
    fn test_memory_pressure_config_defaults() {
        let config = MemoryPressureConfig::default();
        assert!(config.adaptive);
        assert_eq!(config.retrieval_min, 3);
        assert_eq!(config.retrieval_max, 15);
        assert!(config.expand_on_churn);
        assert!(config.expand_on_drift);
        assert!(!config.expand_on_correction);
    }

    #[test]
    fn test_context_window_config_defaults() {
        let config = ContextWindowConfig::default();
        assert!(config.adaptive);
        assert!(config.dynamic_compression);
        assert!((config.compression_threshold_min - 0.5).abs() < 0.001);
        assert!((config.compression_threshold_max - 0.95).abs() < 0.001);
        assert!((config.remaining_turn_factor - 0.33).abs() < 0.001);
        assert_eq!(config.error_recovery_reserve, 10_000);
    }

    #[test]
    fn test_runtime_config_has_new_sub_configs() {
        let config = RuntimeConfig::default();
        // Just verify they exist and serialize
        let toml = config.to_toml().unwrap();
        assert!(toml.contains("[verification]"));
        assert!(toml.contains("[memory_pressure]"));
        assert!(toml.contains("[context_window]"));
    }

    #[test]
    fn test_merge_applies_non_default_fields_across_sections() {
        let merged = RuntimeConfig::default().merge(RuntimeConfig {
            version: "2.0".to_string(),
            compression: CompressionConfig {
                max_history_tokens: 12345,
                compression_threshold: 0.65,
                preserve_tool_calls: false,
                preserve_recent_turns: 7,
                max_tool_result_length: 9000,
                strategy: CompressionStrategy::Aggressive,
            },
            memory: MemoryConfig {
                retrieval_top_k: 9,
                min_relevance_score: 0.55,
                session_weight: 1.25,
                long_term_weight: 0.6,
                max_memory_tokens: 8192,
                include_repository_memories: false,
                strategy: MemoryStrategy::Comprehensive,
            },
            tool_selection: ToolSelectionConfig {
                max_tools: 12,
                confidence_threshold: 0.7,
                prefer_recent_tools: false,
                recent_tool_boost: 0.4,
                max_tool_schema_tokens: 22000,
                tool_budget_tokens: 0,
                max_identical_tool_calls: 0,
                max_tools_per_turn: 0,
                round_budget_warning: 0,
                round_budget_limit: 0,
                circuit_breaker_stall_threshold: 0,
                circuit_breaker_repetition_threshold: 0,
                circuit_breaker_half_open_patience: 0,
                circuit_breaker_absolute_max_rounds: 0,
                circuit_breaker_read_only_stall_threshold: 0,
                circuit_breaker_max_introspect_emissions: 0,
                parallel_batching_force_streak: 0,
                redundant_reads_midloop_threshold: 0,
                sequential_read_churn_eval_threshold: 0,
                redundant_reads_eval_threshold: 0,
                search_fanout_eval_threshold: 0,
                redundant_validation_retries_eval_threshold: 0,
                cache_waste_midloop_threshold: 0,
                exploration_family_churn_midloop_threshold: 0,
                model_profiles: Vec::new(),
            },
            trace: SessionTraceConfig {
                profile: TraceProfile::Custom,
                min_level: TraceLevelSerde::Debug,
                enabled_categories: vec![
                    TraceCategory::ToolCalls,
                    TraceCategory::LlmExchanges,
                    TraceCategory::Reflection,
                ],
                sinks: vec![TraceSink::Stderr],
                sampling_rate: 0.5,
            },
            token_budget: TokenBudgetConfig {
                max_prompt_tokens: 16000,
                max_turn_input_tokens: 32000,
                system_prompt_reserve: 2000,
                tools_reserve: 6000,
            },
            verification: VerificationConfig {
                adaptive: false,
                strictness: 0.75,
                min_strictness: 0.3,
                max_strictness: 0.95,
                increase_on_correction: false,
                increase_on_drift: true,
            },
            memory_pressure: MemoryPressureConfig {
                adaptive: false,
                retrieval_min: 4,
                retrieval_max: 20,
                expand_on_churn: false,
                expand_on_drift: false,
                expand_on_correction: true,
            },
            context_window: ContextWindowConfig {
                adaptive: false,
                dynamic_compression: false,
                compression_threshold_min: 0.45,
                compression_threshold_max: 0.98,
                remaining_turn_factor: 0.5,
                error_recovery_reserve: 12000,
                adaptive_budget_reduction: true,
            },
            adaptive_tuning: AdaptiveTuningConfig {
                scenario_cooldown_turns: 10,
                budget_cooldown_turns: 6,
                tuning_cycle_interval: 8,
            },
            safety: SafetyConfig::default(),
            fork_prefix: ForkPrefixConfig::default(),
            tool_surface: ToolSurfaceConfig::default(),
            runtime_limits: RuntimeLimitsConfig {
                max_turns: 250,
                plan_subtask_max_turns: 175,
            },
        });

        assert_eq!(merged.version, "2.0");
        assert_eq!(merged.compression.max_history_tokens, 12345);
        assert!((merged.compression.compression_threshold - 0.65).abs() < 0.001);
        assert!(!merged.compression.preserve_tool_calls);
        assert_eq!(merged.compression.preserve_recent_turns, 7);
        assert_eq!(merged.compression.max_tool_result_length, 9000);
        assert_eq!(merged.compression.strategy, CompressionStrategy::Aggressive);

        assert_eq!(merged.memory.retrieval_top_k, 9);
        assert!((merged.memory.min_relevance_score - 0.55).abs() < 0.001);
        assert!((merged.memory.session_weight - 1.25).abs() < 0.001);
        assert!((merged.memory.long_term_weight - 0.6).abs() < 0.001);
        assert_eq!(merged.memory.max_memory_tokens, 8192);
        assert!(!merged.memory.include_repository_memories);
        assert_eq!(merged.memory.strategy, MemoryStrategy::Comprehensive);

        assert_eq!(merged.tool_selection.max_tools, 12);
        assert!((merged.tool_selection.confidence_threshold - 0.7).abs() < 0.001);
        assert!(!merged.tool_selection.prefer_recent_tools);
        assert!((merged.tool_selection.recent_tool_boost - 0.4).abs() < 0.001);
        assert_eq!(merged.tool_selection.max_tool_schema_tokens, 22000);

        assert_eq!(merged.trace.profile, TraceProfile::Custom);
        assert_eq!(merged.trace.min_level, TraceLevelSerde::Debug);
        assert!(merged.trace.category_enabled(TraceCategory::ToolCalls));
        assert!(merged.trace.category_enabled(TraceCategory::LlmExchanges));
        assert!(!merged.trace.category_enabled(TraceCategory::ContextAssembly));
        assert!(merged.trace.category_enabled(TraceCategory::Reflection));
        assert!(merged.trace.sinks.contains(&TraceSink::Stderr));
        assert!((merged.trace.sampling_rate - 0.5).abs() < 0.001);

        assert_eq!(merged.token_budget.max_prompt_tokens, 16000);
        assert_eq!(merged.token_budget.max_turn_input_tokens, 32000);
        assert_eq!(merged.token_budget.system_prompt_reserve, 2000);
        assert_eq!(merged.token_budget.tools_reserve, 6000);

        assert!(!merged.verification.adaptive);
        assert!((merged.verification.strictness - 0.75).abs() < 0.001);
        assert!((merged.verification.min_strictness - 0.3).abs() < 0.001);
        assert!((merged.verification.max_strictness - 0.95).abs() < 0.001);
        assert!(!merged.verification.increase_on_correction);
        assert!(merged.verification.increase_on_drift);

        assert!(!merged.memory_pressure.adaptive);
        assert_eq!(merged.memory_pressure.retrieval_min, 4);
        assert_eq!(merged.memory_pressure.retrieval_max, 20);
        assert!(!merged.memory_pressure.expand_on_churn);
        assert!(!merged.memory_pressure.expand_on_drift);
        assert!(merged.memory_pressure.expand_on_correction);

        assert!(!merged.context_window.adaptive);
        assert!(!merged.context_window.dynamic_compression);
        assert!((merged.context_window.compression_threshold_min - 0.45).abs() < 0.001);
        assert!((merged.context_window.compression_threshold_max - 0.98).abs() < 0.001);
        assert!((merged.context_window.remaining_turn_factor - 0.5).abs() < 0.001);
        assert_eq!(merged.context_window.error_recovery_reserve, 12000);

        // Adaptive tuning
        assert_eq!(merged.adaptive_tuning.scenario_cooldown_turns, 10);
        assert_eq!(merged.adaptive_tuning.budget_cooldown_turns, 6);
        assert_eq!(merged.adaptive_tuning.tuning_cycle_interval, 8);
    }

    #[test]
    fn round_budget_defaults() {
        // Deprecated: always returns 200 (effectively disabled).
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_round_budget_warning(), 200);
        assert_eq!(cfg.effective_round_budget_limit(), 200);
    }

    #[test]
    fn runtime_turn_ceiling_config_overrides_env() {
        let cfg = RuntimeLimitsConfig {
            max_turns: 8,
            plan_subtask_max_turns: 0,
        };
        assert_eq!(cfg.resolve_turn_ceiling(false), 8);
    }

    #[test]
    fn runtime_turn_ceiling_falls_back_to_env_when_zero() {
        let env_max = astra_core::RuntimeLimits::global().max_turns;
        let cfg = RuntimeLimitsConfig {
            max_turns: 0,
            plan_subtask_max_turns: 0,
        };
        assert_eq!(cfg.resolve_turn_ceiling(false), env_max);
    }

    #[test]
    fn runtime_turn_ceiling_plan_subtask_config_wins() {
        let cfg = RuntimeLimitsConfig {
            max_turns: 100,
            plan_subtask_max_turns: 20,
        };
        assert_eq!(cfg.resolve_turn_ceiling(true), 20);
    }

    #[test]
    fn runtime_turn_ceiling_plan_subtask_falls_back_to_config_max() {
        let cfg = RuntimeLimitsConfig {
            max_turns: 30,
            plan_subtask_max_turns: 0,
        };
        assert_eq!(cfg.resolve_turn_ceiling(true), 30);
    }

    #[test]
    fn runtime_turn_ceiling_plan_subtask_falls_back_to_env() {
        let env_effective = astra_core::RuntimeLimits::global().effective_plan_subtask_turns();
        let cfg = RuntimeLimitsConfig {
            max_turns: 0,
            plan_subtask_max_turns: 0,
        };
        assert_eq!(cfg.resolve_turn_ceiling(true), env_effective);
    }

    #[test]
    fn round_budget_custom_values() {
        // Deprecated: custom values are ignored, always returns 200.
        let cfg = ToolSelectionConfig {
            round_budget_warning: 5,
            round_budget_limit: 10,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_warning(), 200);
        assert_eq!(cfg.effective_round_budget_limit(), 200);
    }

    #[test]
    fn round_budget_limit_enforces_above_warning() {
        // Deprecated: always returns 200 regardless of config.
        let cfg = ToolSelectionConfig {
            round_budget_warning: 8,
            round_budget_limit: 5,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_limit(), 200);
    }

    #[test]
    fn round_budget_limit_zero_uses_default_regardless_of_warning() {
        // Deprecated: always returns 200.
        let cfg = ToolSelectionConfig {
            round_budget_warning: 5,
            round_budget_limit: 0,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_limit(), 200);
    }

    #[test]
    fn parallel_batching_force_streak_default_and_floor() {
        // 0 → default 5 (must stay above PARALLEL_BATCHING_NUDGE_THRESHOLD=4
        // so the prompt-side nudge fires before the runtime force corrective).
        let cfg = ToolSelectionConfig::default();
        assert_eq!(
            cfg.effective_parallel_batching_force_streak(),
            MIN_PARALLEL_BATCHING_FORCE_STREAK
        );
        // explicit override respected
        let cfg = ToolSelectionConfig {
            parallel_batching_force_streak: 8,
            ..Default::default()
        };
        assert_eq!(cfg.effective_parallel_batching_force_streak(), 8);
        let cfg = ToolSelectionConfig {
            parallel_batching_force_streak: u32::MAX,
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_parallel_batching_force_streak(),
            MAX_PARALLEL_BATCHING_FORCE_STREAK
        );
        // pathological override 1 floors to MIN_PARALLEL_BATCHING_FORCE_STREAK
        // (5 — strictly above PARALLEL_BATCHING_NUDGE_THRESHOLD=4 to preserve
        // the soft→hard cascade).
        let cfg = ToolSelectionConfig {
            parallel_batching_force_streak: 1,
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_parallel_batching_force_streak(),
            MIN_PARALLEL_BATCHING_FORCE_STREAK
        );
        // any value at or below the nudge threshold (4) is clamped up
        for low in 2..=4 {
            let cfg = ToolSelectionConfig {
                parallel_batching_force_streak: low,
                ..Default::default()
            };
            assert_eq!(
                cfg.effective_parallel_batching_force_streak(),
                MIN_PARALLEL_BATCHING_FORCE_STREAK,
                "force-streak override {low} must clamp above the nudge threshold"
            );
        }
    }

    #[test]
    fn resolve_for_model_parallel_batching_force_profile_uses_floor() {
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "flash".to_string(),
            parallel_batching_force_streak: 1,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("deepseek-v4-flash"));
        assert_eq!(
            policy.parallel_batching_force_streak,
            MIN_PARALLEL_BATCHING_FORCE_STREAK
        );

        // Mid-range pathological overrides must also clamp — a profile that
        // sets force=4 would let the hard corrective fire on the same round
        // the prompt-layer first nudges, breaking the cascade.
        for low in 2..=4 {
            let mut cfg = ToolSelectionConfig::default();
            cfg.model_profiles.push(ModelPolicyProfile {
                model_match: "flash".to_string(),
                parallel_batching_force_streak: low,
                ..Default::default()
            });
            let policy = cfg.resolve_for_model(Some("deepseek-v4-flash"));
            assert_eq!(
                policy.parallel_batching_force_streak, MIN_PARALLEL_BATCHING_FORCE_STREAK,
                "per-profile force-streak override {low} must clamp above the nudge threshold"
            );
        }

        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "flash".to_string(),
            parallel_batching_force_streak: u32::MAX,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("deepseek-v4-flash"));
        assert_eq!(
            policy.parallel_batching_force_streak,
            MAX_PARALLEL_BATCHING_FORCE_STREAK
        );
    }

    /// Cross-crate invariant: the per-profile floor MUST equal the global
    /// floor, otherwise a user can configure one path below the nudge
    /// threshold and silently invert the soft→hard cascade.
    #[test]
    fn global_and_profile_floors_for_parallel_batching_force_agree() {
        let global = ToolSelectionConfig {
            parallel_batching_force_streak: 1,
            ..Default::default()
        }
        .effective_parallel_batching_force_streak();

        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "flash".to_string(),
            parallel_batching_force_streak: 1,
            ..Default::default()
        });
        let via_profile = cfg
            .resolve_for_model(Some("deepseek-v4-flash"))
            .parallel_batching_force_streak;

        assert_eq!(
            global, via_profile,
            "floor for parallel_batching_force_streak must be symmetric \
             between global and per-profile paths"
        );
    }

    #[test]
    fn redundant_reads_midloop_threshold_default_and_floor() {
        // 0 → default 4
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_redundant_reads_midloop_threshold(), 4);
        // explicit override respected
        let cfg = ToolSelectionConfig {
            redundant_reads_midloop_threshold: 6,
            ..Default::default()
        };
        assert_eq!(cfg.effective_redundant_reads_midloop_threshold(), 6);
        // pathological override 1 floors to 2
        let cfg = ToolSelectionConfig {
            redundant_reads_midloop_threshold: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_redundant_reads_midloop_threshold(), 2);
    }

    #[test]
    fn sequential_read_churn_eval_threshold_default_and_floor() {
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_sequential_read_churn_eval_threshold(), 8);

        let cfg = ToolSelectionConfig {
            sequential_read_churn_eval_threshold: 10,
            ..Default::default()
        };
        assert_eq!(cfg.effective_sequential_read_churn_eval_threshold(), 10);

        let cfg = ToolSelectionConfig {
            sequential_read_churn_eval_threshold: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_sequential_read_churn_eval_threshold(), 2);
    }

    #[test]
    fn redundant_reads_eval_threshold_default_and_floor() {
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_redundant_reads_eval_threshold(), 3);

        let cfg = ToolSelectionConfig {
            redundant_reads_eval_threshold: 6,
            ..Default::default()
        };
        assert_eq!(cfg.effective_redundant_reads_eval_threshold(), 6);

        let cfg = ToolSelectionConfig {
            redundant_reads_eval_threshold: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_redundant_reads_eval_threshold(), 2);
    }

    #[test]
    fn search_fanout_eval_threshold_default_and_floor() {
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_search_fanout_eval_threshold(), 8);

        let cfg = ToolSelectionConfig {
            search_fanout_eval_threshold: 10,
            ..Default::default()
        };
        assert_eq!(cfg.effective_search_fanout_eval_threshold(), 10);

        let cfg = ToolSelectionConfig {
            search_fanout_eval_threshold: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_search_fanout_eval_threshold(), 2);
    }

    #[test]
    fn redundant_validation_retries_eval_threshold_default_and_override() {
        let cfg = ToolSelectionConfig::default();
        assert_eq!(
            cfg.effective_redundant_validation_retries_eval_threshold(),
            2
        );

        let cfg = ToolSelectionConfig {
            redundant_validation_retries_eval_threshold: 4,
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_redundant_validation_retries_eval_threshold(),
            4
        );

        let cfg = ToolSelectionConfig {
            redundant_validation_retries_eval_threshold: 1,
            ..Default::default()
        };
        assert_eq!(
            cfg.effective_redundant_validation_retries_eval_threshold(),
            1
        );
    }

    #[test]
    fn resolve_threshold_helper() {
        // 0 → default
        assert_eq!(super::resolve_threshold(0, 5, 2), 5);
        // explicit value above floor → as-is
        assert_eq!(super::resolve_threshold(8, 5, 2), 8);
        // explicit value below floor → clamped to floor
        assert_eq!(super::resolve_threshold(1, 5, 2), 2);
        // explicit value == floor → as-is
        assert_eq!(super::resolve_threshold(2, 5, 2), 2);
        // floor of 1 (validation retries case)
        assert_eq!(super::resolve_threshold(1, 2, 1), 1);
    }

    #[test]
    fn effective_max_identical_calls_default_is_three() {
        // Raised from 2 on 2026-04-27 — update the doc in
        // `effective_max_identical_calls` if this changes.
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_max_identical_calls(), 3);
    }

    #[test]
    fn resolve_for_model_without_model_id_uses_global_default() {
        let cfg = ToolSelectionConfig::default();
        let policy = cfg.resolve_for_model(None);
        assert_eq!(policy.max_identical_tool_calls, 3);
        assert_eq!(policy.max_tools_per_turn, 15);
    }

    #[test]
    fn resolve_for_model_hits_builtin_opus_profile() {
        let cfg = ToolSelectionConfig::default();
        // Full Bedrock-style id with "opus" embedded.
        let policy = cfg.resolve_for_model(Some("us.anthropic.claude-opus-4-7-v1"));
        assert_eq!(policy.max_identical_tool_calls, 4);
        assert_eq!(policy.max_tools_per_turn, 20);
    }

    #[test]
    fn resolve_for_model_builtin_haiku_keeps_conservative() {
        let cfg = ToolSelectionConfig::default();
        let policy = cfg.resolve_for_model(Some("claude-haiku-4-5-20251001"));
        assert_eq!(policy.max_identical_tool_calls, 2);
        assert_eq!(policy.max_tools_per_turn, 12);
    }

    #[test]
    fn resolve_for_model_unknown_falls_back_to_global() {
        let cfg = ToolSelectionConfig::default();
        let policy = cfg.resolve_for_model(Some("some-obscure-model-id"));
        // No built-in match → global defaults (3 / 15).
        assert_eq!(policy.max_identical_tool_calls, 3);
        assert_eq!(policy.max_tools_per_turn, 15);
    }

    #[test]
    fn resolve_for_model_user_profile_overrides_builtin() {
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "opus".to_string(),
            max_identical_tool_calls: 8,
            max_tools_per_turn: 0, // 0 → inherit global
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("claude-opus-4-7"));
        // User override wins over built-in.
        assert_eq!(policy.max_identical_tool_calls, 8);
        // Field left at 0 inherits the global default (not the built-in 20).
        assert_eq!(policy.max_tools_per_turn, 15);
    }

    #[test]
    fn resolve_for_model_empty_pattern_matches_any() {
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: String::new(),
            max_identical_tool_calls: 7,
            max_tools_per_turn: 0,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("anything-at-all"));
        assert_eq!(policy.max_identical_tool_calls, 7);
    }

    #[test]
    fn resolve_for_model_match_is_case_insensitive() {
        let cfg = ToolSelectionConfig::default();
        let policy = cfg.resolve_for_model(Some("CLAUDE-OPUS-4-7"));
        assert_eq!(policy.max_identical_tool_calls, 4);
    }

    #[test]
    fn resolve_for_model_floor_applied_to_user_override() {
        // Floor of 5 for max_tools_per_turn — defense against misconfig.
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "custom".to_string(),
            max_identical_tool_calls: 0,
            max_tools_per_turn: 2, // below floor
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("custom-model"));
        assert_eq!(policy.max_tools_per_turn, 5);
    }

    #[test]
    fn resolve_for_model_rejects_too_short_pattern_as_footgun() {
        // `model_match = "4"` would match any model containing a "4"
        // (claude-opus-4-7, gpt-4, etc.) — almost certainly a misconfig
        // rather than intent. Require ≥ 3 chars. Empty string stays the
        // explicit fallback-profile sentinel and is unaffected.
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "4".to_string(),
            max_identical_tool_calls: 99,
            ..Default::default()
        });
        // Should NOT match — too-short pattern ignored.
        let policy = cfg.resolve_for_model(Some("claude-opus-4-7"));
        assert_ne!(
            policy.max_identical_tool_calls, 99,
            "single-char pattern must not match — it's a footgun"
        );
        // Built-in opus profile should still win.
        assert_eq!(policy.max_identical_tool_calls, 4);
    }

    #[test]
    fn resolve_for_model_rejects_two_char_pattern() {
        // Boundary: "op" is still too short (most pathological match cases
        // — "o", "4", "us" — are 1–2 chars).
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "op".to_string(),
            max_identical_tool_calls: 99,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("claude-opus-4-7"));
        assert_ne!(policy.max_identical_tool_calls, 99);
    }

    #[test]
    fn resolve_for_model_accepts_three_char_pattern() {
        // Boundary: 3 chars is the minimum allowed — "gpt", "opus" minus
        // one, etc. Honoring this lets users target narrower model families.
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "4-7".to_string(),
            max_identical_tool_calls: 7,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("claude-opus-4-7"));
        assert_eq!(policy.max_identical_tool_calls, 7);
    }

    #[test]
    fn resolve_for_model_floor_applied_to_max_identical_tool_calls() {
        // A user profile with max_identical_tool_calls = 1 would turn every
        // tool call after the first into a "duplicate" — indistinguishable
        // from "disable tool use entirely after N=1". That's almost
        // certainly a misconfig; clamp to the lowest value any built-in
        // profile uses (2, matching haiku).
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "custom".to_string(),
            max_identical_tool_calls: 1,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("custom-model"));
        assert_eq!(
            policy.max_identical_tool_calls, 2,
            "max_identical_tool_calls=1 should be clamped to floor 2"
        );
    }

    #[test]
    fn resolve_for_model_empty_pattern_still_works_as_fallback() {
        // Regression guard: after tightening the min-length check, the
        // empty-string fallback-profile pattern must still match any model.
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: String::new(),
            max_identical_tool_calls: 9,
            ..Default::default()
        });
        let policy = cfg.resolve_for_model(Some("anything"));
        assert_eq!(policy.max_identical_tool_calls, 9);
    }

    #[test]
    fn rejected_model_match_patterns_lists_short_patterns_preserves_order() {
        // Intent: `show-policy` can read this list verbatim and tell the
        // user "these patterns are being ignored".
        let mut cfg = ToolSelectionConfig::default();
        for p in ["4", "op", "opus", "", "us"] {
            cfg.model_profiles.push(ModelPolicyProfile {
                model_match: p.to_string(),
                ..Default::default()
            });
        }
        let rejected = cfg.rejected_model_match_patterns();
        // Short non-empty patterns surfaced, in declaration order.
        // "opus" (valid) and "" (explicit fallback) must be filtered out.
        assert_eq!(
            rejected,
            vec!["4".to_string(), "op".to_string(), "us".to_string()]
        );
    }

    #[test]
    fn rejected_model_match_patterns_empty_when_all_valid() {
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "gpt-5".to_string(),
            ..Default::default()
        });
        assert!(cfg.rejected_model_match_patterns().is_empty());
    }

    #[test]
    fn model_profiles_round_trip_through_toml() {
        let mut cfg = RuntimeConfig::default();
        cfg.tool_selection.model_profiles.push(ModelPolicyProfile {
            model_match: "gpt-5".to_string(),
            max_identical_tool_calls: 6,
            max_tools_per_turn: 25,
            repeated_cache_hit_suppression: 0,
            max_consecutive_empty_name: 0,
            parallel_batching_force_streak: 0,
        });
        let toml = cfg.to_toml().unwrap();
        assert!(toml.contains("model_profiles"));
        assert!(toml.contains("gpt-5"));
        let parsed: RuntimeConfig = toml::from_str(&toml).unwrap();
        let profile = &parsed.tool_selection.model_profiles[0];
        assert_eq!(profile.model_match, "gpt-5");
        assert_eq!(profile.max_identical_tool_calls, 6);
    }

    #[test]
    fn safety_config_defaults_to_strict_trust_mode() {
        // TrustMode::Strict is the safe default. Shipping with Trusted would
        // turn a fail-closed guard into fail-open — never do that implicitly.
        let cfg = RuntimeConfig::default();
        // The resolved value is what every caller reads.
        assert_eq!(cfg.safety.resolved_trust_mode(), TrustModeSerde::Strict);
    }

    #[test]
    fn safety_config_default_has_no_explicit_trust_mode() {
        // `resolved_trust_mode()` defaults to Strict, but the raw field
        // distinguishes "unset" from "explicit strict" — this matters for
        // merge semantics (see the merge tests).
        let cfg = RuntimeConfig::default();
        assert!(
            cfg.safety.trust_mode.is_none(),
            "default must be None so layered configs can explicitly set Strict too"
        );
    }

    /// `TrustModeSerde` is a unit-variant enum — making it `Copy` avoids
    /// `.clone()` on the read path and is a typical shape for small
    /// config enums. This is a compile-time assertion; if `Copy` is
    /// dropped, `fn requires<T: Copy>() {}` stops compiling.
    #[test]
    fn trust_mode_serde_is_copy() {
        fn requires_copy<T: Copy>() {}
        requires_copy::<TrustModeSerde>();
    }

    #[test]
    fn safety_trust_mode_parses_from_toml() {
        let toml = r#"
            version = "1.0"
            [safety]
            trust_mode = "trusted"
        "#;
        let parsed: RuntimeConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.safety.resolved_trust_mode(), TrustModeSerde::Trusted);
        assert_eq!(parsed.safety.trust_mode, Some(TrustModeSerde::Trusted));
    }

    #[test]
    fn safety_trust_mode_explicit_strict_parses_as_some() {
        // "strict" in the TOML must round-trip as Some(Strict), not None —
        // so a later merge can use the explicit intent.
        let toml = r#"
            version = "1.0"
            [safety]
            trust_mode = "strict"
        "#;
        let parsed: RuntimeConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.safety.trust_mode, Some(TrustModeSerde::Strict));
    }

    #[test]
    fn safety_trust_mode_rejects_unknown_values_by_serde() {
        let toml = r#"
            version = "1.0"
            [safety]
            trust_mode = "unsafe"
        "#;
        let result: Result<RuntimeConfig, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown trust_mode should fail to parse");
    }

    #[test]
    fn safety_merge_project_trusted_overrides_user_strict() {
        // Layered: user = unset (Strict), project = explicit Trusted.
        // Later layer wins (standard config convention).
        let user = RuntimeConfig::default();
        let mut project = RuntimeConfig::default();
        project.safety.trust_mode = Some(TrustModeSerde::Trusted);

        let merged = user.merge(project);
        assert_eq!(merged.safety.resolved_trust_mode(), TrustModeSerde::Trusted);
    }

    #[test]
    fn safety_merge_project_strict_overrides_user_trusted() {
        // The formerly-broken direction: user set Trusted, project explicitly
        // wants Strict. Project must win — a checked-in project config
        // should be able to re-tighten a locally-loose user setting.
        let mut user = RuntimeConfig::default();
        user.safety.trust_mode = Some(TrustModeSerde::Trusted);

        let mut project = RuntimeConfig::default();
        project.safety.trust_mode = Some(TrustModeSerde::Strict);

        let merged = user.merge(project);
        assert_eq!(
            merged.safety.resolved_trust_mode(),
            TrustModeSerde::Strict,
            "explicit Strict in later layer must override earlier Trusted"
        );
    }

    #[test]
    fn safety_merge_project_unset_preserves_user_trusted() {
        // Project doesn't mention safety → user's explicit Trusted sticks.
        let mut user = RuntimeConfig::default();
        user.safety.trust_mode = Some(TrustModeSerde::Trusted);
        let project = RuntimeConfig::default(); // unset

        let merged = user.merge(project);
        assert_eq!(merged.safety.resolved_trust_mode(), TrustModeSerde::Trusted);
    }

    #[test]
    fn effective_policy_exposes_cache_hit_suppression_and_empty_name_limits() {
        // Global default: suppression threshold = 3, empty-name cap = 3.
        // These replaced the hardcoded `REPEATED_CACHE_HIT_SUPPRESSION_THRESHOLD`
        // and `MAX_CONSECUTIVE_EMPTY_NAME` constants in the runtime pipeline.
        let cfg = ToolSelectionConfig::default();
        let policy = cfg.resolve_for_model(None);
        assert_eq!(policy.repeated_cache_hit_suppression, 3);
        assert_eq!(policy.max_consecutive_empty_name, 3);
    }

    #[test]
    fn opus_profile_loosens_cache_hit_suppression() {
        // Stronger models repeat reads more deliberately — give them more rope
        // before suppression kicks in. Haiku stays tight.
        let cfg = ToolSelectionConfig::default();
        let opus = cfg.resolve_for_model(Some("claude-opus-4-7"));
        assert_eq!(opus.repeated_cache_hit_suppression, 4);

        let haiku = cfg.resolve_for_model(Some("claude-haiku-4-5"));
        assert_eq!(haiku.repeated_cache_hit_suppression, 2);
    }

    #[test]
    fn user_profile_overrides_new_fields_independently() {
        let mut cfg = ToolSelectionConfig::default();
        cfg.model_profiles.push(ModelPolicyProfile {
            model_match: "custom".to_string(),
            max_identical_tool_calls: 0,
            max_tools_per_turn: 0,
            repeated_cache_hit_suppression: 5,
            max_consecutive_empty_name: 4,
            parallel_batching_force_streak: 0,
        });
        let policy = cfg.resolve_for_model(Some("custom-model"));
        // These two override…
        assert_eq!(policy.repeated_cache_hit_suppression, 5);
        assert_eq!(policy.max_consecutive_empty_name, 4);
        // …while the zero-valued fields inherit the global default.
        assert_eq!(policy.max_identical_tool_calls, 3);
        assert_eq!(policy.max_tools_per_turn, 15);
    }

    // ─── Fork-prefix config ─────────────────────────────────────────

    #[test]
    fn fork_prefix_defaults_to_disabled_noop_sink() {
        let cfg = ForkPrefixConfig::default();
        assert!(!cfg.enabled, "fork-prefix must default to disabled");
        assert_eq!(cfg.sink, ForkCacheSinkKind::Noop);
        assert!((cfg.hit_threshold - 0.80).abs() < 1e-9);
        assert!((cfg.miss_floor - 0.05).abs() < 1e-9);
    }

    #[test]
    fn fork_prefix_parses_from_toml() {
        let toml_str = r#"
            version = "1.0"

            [fork_prefix]
            enabled = true
            sink = "stderr"
            hit_threshold = 0.85
            miss_floor = 0.10
        "#;
        let cfg: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.fork_prefix.enabled);
        assert_eq!(cfg.fork_prefix.sink, ForkCacheSinkKind::Stderr);
        assert!((cfg.fork_prefix.hit_threshold - 0.85).abs() < 1e-9);
        assert!((cfg.fork_prefix.miss_floor - 0.10).abs() < 1e-9);
    }

    #[test]
    fn fork_prefix_missing_section_uses_defaults() {
        // A TOML without the section must not fail — defaults fill in.
        let toml_str = r#"version = "1.0""#;
        let cfg: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.fork_prefix.enabled);
        assert_eq!(cfg.fork_prefix.sink, ForkCacheSinkKind::Noop);
    }

    #[test]
    fn fork_prefix_sink_rename_to_lowercase() {
        // Serialization uses lowercase literals for readability. The
        // tripwire pins both directions so a future rename to e.g.
        // `SnakeCase` is an explicit breaking config change.
        let s = toml::to_string(&ForkPrefixConfig {
            enabled: true,
            sink: ForkCacheSinkKind::Stderr,
            ..Default::default()
        })
        .unwrap();
        assert!(
            s.contains("sink = \"stderr\""),
            "expected lowercase serialization, got {s}"
        );
    }

    #[test]
    fn fork_prefix_enabled_field_is_deprecated_noop() {
        // The `enabled` field remains for backward-compatible TOML
        // deserialization but has no runtime effect. Fork capture is
        // always-on.
        let cfg = ForkPrefixConfig::default();
        // Default value doesn't matter for runtime behavior since
        // the field is a no-op; we just verify it deserializes.
        let _ = cfg.enabled;
    }
}
