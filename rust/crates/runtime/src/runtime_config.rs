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
            if let Ok(content) = std::fs::read_to_string(&user_config) {
                if let Ok(user) = toml::from_str::<RuntimeConfig>(&content) {
                    config = config.merge(user);
                }
            }
        }

        // Project-level config
        let project_config = PathBuf::from(".astra/config/runtime.toml");
        if let Ok(content) = std::fs::read_to_string(&project_config) {
            if let Ok(project) = toml::from_str::<RuntimeConfig>(&content) {
                config = config.merge(project);
            }
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
        // Simple field-by-field merge (could be more sophisticated)
        if other.compression.max_history_tokens != default_max_history_tokens() {
            self.compression.max_history_tokens = other.compression.max_history_tokens;
        }
        if other.compression.compression_threshold != default_compression_threshold() {
            self.compression.compression_threshold = other.compression.compression_threshold;
        }
        if other.compression.strategy != CompressionStrategy::default() {
            self.compression.strategy = other.compression.strategy;
        }
        // ... more fields as needed
        self
    }

    /// Apply environment variable overrides.
    fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("MO_MAX_HISTORY_TOKENS") {
            if let Ok(n) = val.parse() {
                self.compression.max_history_tokens = n;
            }
        }
        if let Ok(val) = std::env::var("MO_COMPRESSION_THRESHOLD") {
            if let Ok(n) = val.parse() {
                self.compression.compression_threshold = n;
            }
        }
        if let Ok(val) = std::env::var("MO_RETRIEVAL_TOP_K") {
            if let Ok(n) = val.parse() {
                self.memory.retrieval_top_k = n;
            }
        }
        if let Ok(val) = std::env::var("MO_MAX_TURN_INPUT_TOKENS") {
            if let Ok(n) = val.parse() {
                self.token_budget.max_turn_input_tokens = n;
            }
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
}
