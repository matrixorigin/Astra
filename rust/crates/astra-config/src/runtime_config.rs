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
//! 4. Environment variables (MO_CONFIG_*)
//! 5. Runtime overrides (via API)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

    /// Learning/adaptation configuration.
    #[serde(default)]
    pub learning: LearningConfig,

    /// Telemetry configuration.
    #[serde(default)]
    pub telemetry: TelemetryConfig,

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
            learning: LearningConfig::default(),
            telemetry: TelemetryConfig::default(),
            token_budget: TokenBudgetConfig::default(),
            verification: VerificationConfig::default(),
            memory_pressure: MemoryPressureConfig::default(),
            context_window: ContextWindowConfig::default(),
            adaptive_tuning: AdaptiveTuningConfig::default(),
        }
    }
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

    /// Whether to use learned patterns for tool selection.
    #[serde(default = "default_true")]
    pub use_learned_patterns: bool,

    /// Maximum tokens for tool schemas.
    #[serde(default = "default_max_tool_schema_tokens")]
    pub max_tool_schema_tokens: u32,

    /// Scenario-driven override for the tool selection token budget.
    /// 0 = use registry default (800 tokens). Non-zero values override the
    /// `DEFAULT_TOOL_BUDGET_TOKENS` constant in the tool selector, allowing
    /// scenarios to allocate more or fewer tokens for dynamic tool schemas.
    #[serde(default)]
    pub tool_budget_tokens: u32,

    /// Legacy: model name for the removed CLI LLM-based tool selector.
    ///
    /// The `astra-cli` REPL and background plan executor use the TF-IDF tool selector only
    /// (no extra LLM call before the task model). This field is still parsed from TOML for
    /// backward compatibility but has **no effect** on tool selection today.
    #[serde(default)]
    pub selector_model: Option<String>,

    /// Max times the same (tool, args) can execute across a session.
    /// 0 = use default (2). Prevents infinite loops from ignored dedup hints.
    #[serde(default)]
    pub max_identical_tool_calls: u32,

    /// Max tool calls to execute in a single LLM turn (headless round).
    /// 0 = use default (15). Excess calls are skipped with a budget stub.
    /// Prevents pathological turns where the agent requests 50+ tool calls.
    #[serde(default)]
    pub max_tools_per_turn: u32,

    /// Round index at which the LLM receives a "wrap up" warning.
    /// 0 = use default (3). Set higher for complex multi-file tasks.
    #[serde(default)]
    pub round_budget_warning: u32,

    /// Round index at which the LLM is forced to stop calling tools.
    /// 0 = use default (6). Set higher for complex multi-file tasks.
    #[serde(default)]
    pub round_budget_limit: u32,

    /// Mid-loop guard: number of consecutive single-tool rounds tolerated
    /// before the runtime injects a parallel-batching corrective. 0 = use
    /// default (5). Lower values intervene more aggressively; higher values
    /// give the model more rope before correction.
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
}

impl ToolSelectionConfig {
    /// Resolved max identical tool calls (0 → default of 2).
    pub fn effective_max_identical_calls(&self) -> u32 {
        if self.max_identical_tool_calls > 0 {
            self.max_identical_tool_calls
        } else {
            2
        }
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

    /// Resolved round budget warning threshold (0 → default of 8).
    pub fn effective_round_budget_warning(&self) -> u32 {
        if self.round_budget_warning > 0 {
            self.round_budget_warning
        } else {
            8
        }
    }

    /// Resolved round budget hard limit (0 → default of 45).
    pub fn effective_round_budget_limit(&self) -> u32 {
        if self.round_budget_limit > 0 {
            self.round_budget_limit
                .max(self.effective_round_budget_warning() + 1)
        } else {
            45
        }
    }

    /// Resolved parallel-batching force streak threshold (0 → default of 5).
    /// Floor of 2 prevents a misconfiguration from triggering on every round.
    pub fn effective_parallel_batching_force_streak(&self) -> u32 {
        resolve_threshold(self.parallel_batching_force_streak, 5, 2)
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
            use_learned_patterns: default_true(),
            max_tool_schema_tokens: default_max_tool_schema_tokens(),
            tool_budget_tokens: 0,
            selector_model: None,
            max_identical_tool_calls: 0,
            max_tools_per_turn: 0,
            round_budget_warning: 0,
            round_budget_limit: 0,
            parallel_batching_force_streak: 0,
            redundant_reads_midloop_threshold: 0,
            sequential_read_churn_eval_threshold: 0,
            redundant_reads_eval_threshold: 0,
            search_fanout_eval_threshold: 0,
            redundant_validation_retries_eval_threshold: 0,
            cache_waste_midloop_threshold: 0,
            exploration_family_churn_midloop_threshold: 0,
        }
    }
}

// ─── Learning Configuration ──────────────────────────────────────────────────

/// Configuration for learning and adaptation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    /// Whether learning is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Entity decay half-life in days.
    #[serde(default = "default_entity_decay_half_life")]
    pub entity_decay_half_life_days: u32,

    /// Pattern decay half-life in days.
    #[serde(default = "default_pattern_decay_half_life")]
    pub pattern_decay_half_life_days: u32,

    /// Minimum samples before calibration is applied.
    #[serde(default = "default_min_calibration_samples")]
    pub min_calibration_samples: u32,

    /// Exploration rate for tool chain patterns (epsilon-greedy).
    #[serde(default = "default_exploration_rate")]
    pub exploration_rate: f64,

    /// Whether to apply progressive calibration.
    #[serde(default = "default_true")]
    pub progressive_calibration: bool,
}

fn default_entity_decay_half_life() -> u32 {
    60
}
fn default_pattern_decay_half_life() -> u32 {
    30
}
fn default_min_calibration_samples() -> u32 {
    5
}
fn default_exploration_rate() -> f64 {
    0.1
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            entity_decay_half_life_days: default_entity_decay_half_life(),
            pattern_decay_half_life_days: default_pattern_decay_half_life(),
            min_calibration_samples: default_min_calibration_samples(),
            exploration_rate: default_exploration_rate(),
            progressive_calibration: true,
        }
    }
}

// ─── Telemetry Configuration ─────────────────────────────────────────────────

/// Configuration for telemetry and tracing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Whether to capture context assembly traces.
    #[serde(default = "default_true")]
    pub capture_context_traces: bool,

    /// Whether to capture tool execution traces.
    #[serde(default = "default_true")]
    pub capture_tool_traces: bool,

    /// Whether to capture decision explanations.
    #[serde(default)]
    pub capture_explanations: bool,

    /// Maximum traces to keep in memory (reserved for future use).
    /// Currently traces are per-turn and cleared after persist to journal,
    /// so no in-memory buffer accumulates. This config will apply if/when
    /// a diagnostic trace buffer is added for streaming or `/debug traces`.
    #[serde(default = "default_max_traces_in_memory")]
    pub max_traces_in_memory: u32,

    /// Whether to persist traces to journal.
    #[serde(default = "default_true")]
    pub persist_to_journal: bool,
}

fn default_max_traces_in_memory() -> u32 {
    100
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            capture_context_traces: true,
            capture_tool_traces: true,
            capture_explanations: false,
            max_traces_in_memory: default_max_traces_in_memory(),
            persist_to_journal: true,
        }
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
    80000
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

// ─── Configuration Loading ───────────────────────────────────────────────────

impl RuntimeConfig {
    /// Load configuration from default paths.
    ///
    /// Loads in order (later overrides earlier):
    /// 1. Built-in defaults
    /// 2. ~/.astra/config/runtime.toml
    /// 3. .astra/config/runtime.toml
    /// 4. Environment variables
    pub fn load() -> Self {
        let mut config = Self::default();

        // User-level config
        if let Some(home) = dirs::home_dir() {
            let user_config = home.join(".astra/config/runtime.toml");
            if let Ok(content) = std::fs::read_to_string(&user_config)
                && let Ok(user) = toml::from_str::<RuntimeConfig>(&content)
            {
                config = config.merge(user);
            }
        }

        // Project-level config
        let project_config = PathBuf::from(".astra/config/runtime.toml");
        if let Ok(content) = std::fs::read_to_string(&project_config)
            && let Ok(project) = toml::from_str::<RuntimeConfig>(&content)
        {
            config = config.merge(project);
        }

        // Environment overrides
        config.apply_env_overrides();

        // Apply strategy presets
        if config.compression.strategy != CompressionStrategy::Custom {
            let strategy = config.compression.strategy.clone();
            strategy.apply_to(&mut config.compression);
        }

        config
    }

    /// Merge another config into this one (other takes precedence).
    pub fn merge(mut self, other: RuntimeConfig) -> Self {
        let RuntimeConfig {
            version,
            compression,
            memory,
            tool_selection,
            learning,
            telemetry,
            token_budget,
            verification,
            memory_pressure,
            context_window,
            adaptive_tuning,
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
            use_learned_patterns,
            max_tool_schema_tokens,
            tool_budget_tokens,
            selector_model,
            max_identical_tool_calls,
            max_tools_per_turn,
            round_budget_warning,
            round_budget_limit,
            parallel_batching_force_streak,
            redundant_reads_midloop_threshold,
            sequential_read_churn_eval_threshold,
            redundant_reads_eval_threshold,
            search_fanout_eval_threshold,
            redundant_validation_retries_eval_threshold,
            cache_waste_midloop_threshold,
            exploration_family_churn_midloop_threshold,
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
            &mut self.tool_selection.use_learned_patterns,
            use_learned_patterns,
            default_true(),
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
        if selector_model.is_some() {
            self.tool_selection.selector_model = selector_model;
        }
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

        let LearningConfig {
            enabled,
            entity_decay_half_life_days,
            pattern_decay_half_life_days,
            min_calibration_samples,
            exploration_rate,
            progressive_calibration,
        } = learning;
        merge_if_non_default(&mut self.learning.enabled, enabled, default_true());
        merge_if_non_default(
            &mut self.learning.entity_decay_half_life_days,
            entity_decay_half_life_days,
            default_entity_decay_half_life(),
        );
        merge_if_non_default(
            &mut self.learning.pattern_decay_half_life_days,
            pattern_decay_half_life_days,
            default_pattern_decay_half_life(),
        );
        merge_if_non_default(
            &mut self.learning.min_calibration_samples,
            min_calibration_samples,
            default_min_calibration_samples(),
        );
        merge_if_non_default(
            &mut self.learning.exploration_rate,
            exploration_rate,
            default_exploration_rate(),
        );
        merge_if_non_default(
            &mut self.learning.progressive_calibration,
            progressive_calibration,
            default_true(),
        );

        let TelemetryConfig {
            capture_context_traces,
            capture_tool_traces,
            capture_explanations,
            max_traces_in_memory,
            persist_to_journal,
        } = telemetry;
        merge_if_non_default(
            &mut self.telemetry.capture_context_traces,
            capture_context_traces,
            default_true(),
        );
        merge_if_non_default(
            &mut self.telemetry.capture_tool_traces,
            capture_tool_traces,
            default_true(),
        );
        merge_if_non_default(
            &mut self.telemetry.capture_explanations,
            capture_explanations,
            false,
        );
        merge_if_non_default(
            &mut self.telemetry.max_traces_in_memory,
            max_traces_in_memory,
            default_max_traces_in_memory(),
        );
        merge_if_non_default(
            &mut self.telemetry.persist_to_journal,
            persist_to_journal,
            default_true(),
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

        self
    }

    /// Apply environment variable overrides.
    fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("MO_MAX_HISTORY_TOKENS")
            && let Ok(n) = val.parse()
        {
            self.compression.max_history_tokens = n;
        }
        if let Ok(val) = std::env::var("MO_COMPRESSION_THRESHOLD")
            && let Ok(n) = val.parse()
        {
            self.compression.compression_threshold = n;
        }
        if let Ok(val) = std::env::var("MO_RETRIEVAL_TOP_K")
            && let Ok(n) = val.parse()
        {
            self.memory.retrieval_top_k = n;
        }
        if let Ok(val) = std::env::var("MO_MAX_TURN_INPUT_TOKENS")
            && let Ok(n) = val.parse()
        {
            self.token_budget.max_turn_input_tokens = n;
        }
        if let Ok(val) = std::env::var("MO_CAPTURE_TRACES") {
            self.telemetry.capture_context_traces = val == "1" || val.to_lowercase() == "true";
        }
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
    }

    #[test]
    fn test_effective_max_identical_calls() {
        let mut config = ToolSelectionConfig::default();
        assert_eq!(config.effective_max_identical_calls(), 2);

        config.max_identical_tool_calls = 5;
        assert_eq!(config.effective_max_identical_calls(), 5);

        config.max_identical_tool_calls = 1;
        assert_eq!(config.effective_max_identical_calls(), 1);
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
            std::env::set_var("MO_MAX_HISTORY_TOKENS", "50000");
        }
        let mut config = RuntimeConfig::default();
        config.apply_env_overrides();
        assert_eq!(config.compression.max_history_tokens, 50000);
        unsafe {
            std::env::remove_var("MO_MAX_HISTORY_TOKENS");
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
                use_learned_patterns: false,
                max_tool_schema_tokens: 22000,
                tool_budget_tokens: 0,
                selector_model: None,
                max_identical_tool_calls: 0,
                max_tools_per_turn: 0,
                round_budget_warning: 0,
                round_budget_limit: 0,
                parallel_batching_force_streak: 0,
                redundant_reads_midloop_threshold: 0,
                sequential_read_churn_eval_threshold: 0,
                redundant_reads_eval_threshold: 0,
                search_fanout_eval_threshold: 0,
                redundant_validation_retries_eval_threshold: 0,
                cache_waste_midloop_threshold: 0,
                exploration_family_churn_midloop_threshold: 0,
            },
            learning: LearningConfig {
                enabled: false,
                entity_decay_half_life_days: 10,
                pattern_decay_half_life_days: 20,
                min_calibration_samples: 8,
                exploration_rate: 0.25,
                progressive_calibration: false,
            },
            telemetry: TelemetryConfig {
                capture_context_traces: false,
                capture_tool_traces: false,
                capture_explanations: true,
                max_traces_in_memory: 42,
                persist_to_journal: false,
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
            },
            adaptive_tuning: AdaptiveTuningConfig {
                scenario_cooldown_turns: 10,
                budget_cooldown_turns: 6,
                tuning_cycle_interval: 8,
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
        assert!(!merged.tool_selection.use_learned_patterns);
        assert_eq!(merged.tool_selection.max_tool_schema_tokens, 22000);

        assert!(!merged.learning.enabled);
        assert_eq!(merged.learning.entity_decay_half_life_days, 10);
        assert_eq!(merged.learning.pattern_decay_half_life_days, 20);
        assert_eq!(merged.learning.min_calibration_samples, 8);
        assert!((merged.learning.exploration_rate - 0.25).abs() < 0.001);
        assert!(!merged.learning.progressive_calibration);

        assert!(!merged.telemetry.capture_context_traces);
        assert!(!merged.telemetry.capture_tool_traces);
        assert!(merged.telemetry.capture_explanations);
        assert_eq!(merged.telemetry.max_traces_in_memory, 42);
        assert!(!merged.telemetry.persist_to_journal);

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
    fn selector_model_from_toml() {
        let toml = r#"
[tool_selection]
selector_model = "qwen3.5-flash"
"#;
        let cfg: RuntimeConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.tool_selection.selector_model.as_deref(),
            Some("qwen3.5-flash")
        );
    }

    #[test]
    fn selector_model_merge_override() {
        let base = RuntimeConfig::default();
        assert!(base.tool_selection.selector_model.is_none());

        let override_toml = r#"
[tool_selection]
selector_model = "qwen-flash"
"#;
        let overrides: RuntimeConfig = toml::from_str(override_toml).unwrap();
        let merged = base.merge(overrides);
        assert_eq!(
            merged.tool_selection.selector_model.as_deref(),
            Some("qwen-flash")
        );
    }

    #[test]
    fn selector_model_none_does_not_clobber() {
        let mut base = RuntimeConfig::default();
        base.tool_selection.selector_model = Some("qwen3.5-flash".into());

        let empty: RuntimeConfig = toml::from_str("").unwrap();
        let merged = base.merge(empty);
        assert_eq!(
            merged.tool_selection.selector_model.as_deref(),
            Some("qwen3.5-flash")
        );
    }

    #[test]
    fn round_budget_defaults() {
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_round_budget_warning(), 8);
        assert_eq!(cfg.effective_round_budget_limit(), 45);
    }

    #[test]
    fn round_budget_custom_values() {
        let cfg = ToolSelectionConfig {
            round_budget_warning: 5,
            round_budget_limit: 10,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_warning(), 5);
        assert_eq!(cfg.effective_round_budget_limit(), 10);
    }

    #[test]
    fn round_budget_limit_enforces_above_warning() {
        // limit set below warning → clamped to warning + 1
        let cfg = ToolSelectionConfig {
            round_budget_warning: 8,
            round_budget_limit: 5,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_limit(), 9);
    }

    #[test]
    fn round_budget_limit_zero_uses_default_regardless_of_warning() {
        let cfg = ToolSelectionConfig {
            round_budget_warning: 5,
            round_budget_limit: 0,
            ..Default::default()
        };
        assert_eq!(cfg.effective_round_budget_limit(), 45);
    }

    #[test]
    fn parallel_batching_force_streak_default_and_floor() {
        // 0 → default 5
        let cfg = ToolSelectionConfig::default();
        assert_eq!(cfg.effective_parallel_batching_force_streak(), 5);
        // explicit override respected
        let cfg = ToolSelectionConfig {
            parallel_batching_force_streak: 8,
            ..Default::default()
        };
        assert_eq!(cfg.effective_parallel_batching_force_streak(), 8);
        // pathological override 1 floors to 2
        let cfg = ToolSelectionConfig {
            parallel_batching_force_streak: 1,
            ..Default::default()
        };
        assert_eq!(cfg.effective_parallel_batching_force_streak(), 2);
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
}
