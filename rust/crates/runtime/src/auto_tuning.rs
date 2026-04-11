//! M6: Auto-Tuning Loop
//!
//! Automatic parameter adjustment based on feedback signals.
//!
//! Key features:
//! - Evolution rules (trigger → action)
//! - Feedback aggregation (implicit + explicit)
//! - Config adjustment with rollback support
//! - Cooldown and rate limiting

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::lock_ext::RwLockExt;
use crate::pipeline::pattern::PatternLibrary;
use crate::runtime_config::RuntimeConfig;

// ─── Feedback Signals ───────────────────────────────────────────────────────

/// A feedback signal from user behavior or explicit rating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackSignal {
    /// Signal type.
    pub signal_type: SignalType,
    /// When the signal was recorded.
    pub timestamp: SystemTime,
    /// Associated turn ID.
    pub turn_id: Option<String>,
    /// Additional context.
    pub context: HashMap<String, serde_json::Value>,
}

impl FeedbackSignal {
    /// Create a new feedback signal of the given type, timestamped to now.
    pub fn new(signal_type: SignalType) -> Self {
        Self {
            signal_type,
            timestamp: SystemTime::now(),
            turn_id: None,
            context: HashMap::new(),
        }
    }

    /// Associate this signal with a specific turn.
    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// Attach arbitrary context metadata to this signal.
    pub fn with_context(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.context.insert(key.into(), value);
        self
    }
}

/// Types of feedback signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalType {
    // ─── Implicit Signals ───
    /// User retried the same query.
    Retry { count: u32 },
    /// User corrected agent's output.
    Correction,
    /// User interrupted the agent.
    Interruption,
    /// User accepted output without changes.
    Acceptance,
    /// Fast follow-up (indicates engagement).
    QuickFollowUp { delay_ms: u64 },
    /// Long pause before next query (possible confusion).
    LongPause { delay_ms: u64 },

    // ─── Explicit Signals ───
    /// Thumbs up/down rating.
    ThumbsRating { positive: bool },
    /// Numeric rating (1-5 stars).
    StarRating { stars: u8 },
    /// Text feedback.
    TextFeedback { sentiment: Sentiment },

    // ─── Behavioral Signals ───
    /// High token usage for the task.
    HighTokenUsage { tokens: u64, threshold: u64 },
    /// Many tool calls without progress.
    ToolChurn { calls: u32, unique_tools: u32 },
    /// Agent lost focus (drift detected).
    FocusDrift,
    /// Task completed successfully.
    TaskSuccess,
    /// Task failed.
    TaskFailure { reason: String },

    // ─── Tool Health Signals ───
    /// A tool was deprioritized due to repeated failures.
    ToolDeprioritized { tool_name: String },
    /// A tool was rehabilitated after being deprioritized.
    ToolRehabilitated { tool_name: String },
}

impl SignalType {
    // Canonical type names used in `SignalAccumulation` triggers and
    // `count_signals`.  Keep these in sync with `type_name()`.
    pub const NAME_RETRY: &'static str = "retry";
    pub const NAME_CORRECTION: &'static str = "correction";
    pub const NAME_INTERRUPTION: &'static str = "interruption";
    pub const NAME_ACCEPTANCE: &'static str = "acceptance";
    pub const NAME_QUICK_FOLLOW_UP: &'static str = "quick_follow_up";
    pub const NAME_LONG_PAUSE: &'static str = "long_pause";
    pub const NAME_THUMBS_RATING: &'static str = "thumbs_rating";
    pub const NAME_STAR_RATING: &'static str = "star_rating";
    pub const NAME_TEXT_FEEDBACK: &'static str = "text_feedback";
    pub const NAME_HIGH_TOKEN_USAGE: &'static str = "high_token_usage";
    pub const NAME_TOOL_CHURN: &'static str = "tool_churn";
    pub const NAME_FOCUS_DRIFT: &'static str = "focus_drift";
    pub const NAME_TASK_SUCCESS: &'static str = "task_success";
    pub const NAME_TASK_FAILURE: &'static str = "task_failure";
    pub const NAME_TOOL_DEPRIORITIZED: &'static str = "tool_deprioritized";
    pub const NAME_TOOL_REHABILITATED: &'static str = "tool_rehabilitated";

    /// Canonical string name for this signal type, used for accumulation matching.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Retry { .. } => Self::NAME_RETRY,
            Self::Correction => Self::NAME_CORRECTION,
            Self::Interruption => Self::NAME_INTERRUPTION,
            Self::Acceptance => Self::NAME_ACCEPTANCE,
            Self::QuickFollowUp { .. } => Self::NAME_QUICK_FOLLOW_UP,
            Self::LongPause { .. } => Self::NAME_LONG_PAUSE,
            Self::ThumbsRating { .. } => Self::NAME_THUMBS_RATING,
            Self::StarRating { .. } => Self::NAME_STAR_RATING,
            Self::TextFeedback { .. } => Self::NAME_TEXT_FEEDBACK,
            Self::HighTokenUsage { .. } => Self::NAME_HIGH_TOKEN_USAGE,
            Self::ToolChurn { .. } => Self::NAME_TOOL_CHURN,
            Self::FocusDrift => Self::NAME_FOCUS_DRIFT,
            Self::TaskSuccess => Self::NAME_TASK_SUCCESS,
            Self::TaskFailure { .. } => Self::NAME_TASK_FAILURE,
            Self::ToolDeprioritized { .. } => Self::NAME_TOOL_DEPRIORITIZED,
            Self::ToolRehabilitated { .. } => Self::NAME_TOOL_REHABILITATED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    Positive,
    Neutral,
    Negative,
}

// ─── Evolution Rules ────────────────────────────────────────────────────────

/// A rule that triggers automatic configuration changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRule {
    /// Unique rule ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Trigger condition.
    pub trigger: EvolutionTrigger,
    /// Action to take when triggered.
    pub action: EvolutionAction,
    /// Optional rollback condition.
    pub rollback_condition: Option<RollbackCondition>,
    /// Cooldown after triggering.
    pub cooldown: Duration,
    /// Whether this rule is enabled.
    pub enabled: bool,
}

impl EvolutionRule {
    /// Create a new evolution rule with the given trigger and action. Default cooldown: 1 hour.
    pub fn new(id: impl Into<String>, trigger: EvolutionTrigger, action: EvolutionAction) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            trigger,
            action,
            rollback_condition: None,
            cooldown: Duration::from_secs(3600),
            enabled: true,
        }
    }

    /// Set a human-readable display name (defaults to the rule ID).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Attach a rollback condition that reverts this rule's action when met.
    pub fn with_rollback(mut self, condition: RollbackCondition) -> Self {
        self.rollback_condition = Some(condition);
        self
    }

    /// Override the default cooldown period between consecutive triggers.
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }
}

/// Conditions that trigger an evolution rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvolutionTrigger {
    /// Low success rate over a window.
    LowSuccessRate {
        threshold: f64,
        window_secs: u64,
        min_samples: u32,
    },
    /// High token usage over a window.
    HighTokenUsage {
        threshold_tokens: u64,
        window_secs: u64,
        min_samples: u32,
    },
    /// High retry rate.
    HighRetryRate {
        threshold: f64,
        window_secs: u64,
        min_samples: u32,
    },
    /// Consistent negative feedback.
    NegativeFeedbackStreak { count: u32 },
    /// Pattern confidence drop.
    PatternDrift { confidence_drop: f64 },
    /// Custom signal accumulation.
    SignalAccumulation {
        signal_type: String,
        count: u32,
        window_secs: u64,
    },
}

/// Actions to take when a rule triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvolutionAction {
    /// Adjust a config value by delta.
    AdjustConfig {
        path: String,
        delta: f64,
        min: Option<f64>,
        max: Option<f64>,
    },
    /// Set a config value to a specific value.
    SetConfig {
        path: String,
        value: serde_json::Value,
    },
    /// Switch to a different strategy.
    SwitchStrategy {
        strategy_key: String,
        new_value: String,
    },
    /// Enable an A/B experiment.
    EnableExperiment { experiment_id: String },
    /// Disable an A/B experiment.
    DisableExperiment { experiment_id: String },
    /// Reset config to default.
    ResetConfig { path: String },
    /// Log an alert (no config change).
    Alert {
        message: String,
        severity: AlertSeverity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
}

/// Conditions that trigger a rollback of a previous action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RollbackCondition {
    /// Success rate drops below threshold after change.
    SuccessRateDrops { threshold: f64, window_secs: u64 },
    /// Negative feedback increases.
    NegativeFeedbackIncreases { count: u32, window_secs: u64 },
    /// Time limit for evaluation.
    TimeLimit { secs: u64 },
}

// ─── Rule Evaluation ────────────────────────────────────────────────────────

/// Tracks feedback signals for rule evaluation.
#[derive(Debug, Serialize, Deserialize)]
pub struct FeedbackAggregator {
    /// Recent signals.
    signals: VecDeque<FeedbackSignal>,
    /// Max signals to retain.
    max_signals: usize,
    /// Time window for old signal cleanup.
    max_age: Duration,
}

impl Default for FeedbackAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedbackAggregator {
    /// Create a new aggregator with default window (24 h, 1000 signals max).
    pub fn new() -> Self {
        Self {
            signals: VecDeque::new(),
            max_signals: 1000,
            max_age: Duration::from_secs(86400), // 24 hours
        }
    }

    /// Record a new feedback signal.
    pub fn record(&mut self, signal: FeedbackSignal) {
        self.cleanup_old();
        self.signals.push_back(signal);
        if self.signals.len() > self.max_signals {
            self.signals.pop_front();
        }
    }

    /// Get signals within a time window.
    pub fn signals_in_window(&self, window: Duration) -> Vec<&FeedbackSignal> {
        let cutoff = SystemTime::now() - window;
        self.signals
            .iter()
            .filter(|s| s.timestamp >= cutoff)
            .collect()
    }

    /// Return a clone of the retained signal window for inspection/testing.
    pub fn recent_signals(&self) -> Vec<FeedbackSignal> {
        self.signals.iter().cloned().collect()
    }

    /// Count signals of a specific type within a window.
    pub fn count_signals(&self, signal_type: &str, window: Duration) -> u32 {
        let signals = self.signals_in_window(window);
        signals
            .iter()
            .filter(|s| signal_type_matches(&s.signal_type, signal_type))
            .count() as u32
    }

    /// Calculate success rate within a window.
    pub fn success_rate(&self, window: Duration) -> Option<f64> {
        let signals = self.signals_in_window(window);
        let successes = signals
            .iter()
            .filter(|s| {
                matches!(
                    s.signal_type,
                    SignalType::TaskSuccess | SignalType::Acceptance
                )
            })
            .count();
        let failures = signals
            .iter()
            .filter(|s| {
                matches!(
                    s.signal_type,
                    SignalType::TaskFailure { .. }
                        | SignalType::Retry { .. }
                        | SignalType::Correction
                )
            })
            .count();

        let total = successes + failures;
        if total == 0 {
            None
        } else {
            Some(successes as f64 / total as f64)
        }
    }

    /// Calculate average token usage within a window.
    pub fn avg_token_usage(&self, window: Duration) -> Option<u64> {
        let signals = self.signals_in_window(window);
        let token_signals: Vec<u64> = signals
            .iter()
            .filter_map(|s| {
                if let SignalType::HighTokenUsage { tokens, .. } = &s.signal_type {
                    Some(*tokens)
                } else {
                    s.context.get("tokens").and_then(|v| v.as_u64())
                }
            })
            .collect();

        if token_signals.is_empty() {
            None
        } else {
            Some(token_signals.iter().sum::<u64>() / token_signals.len() as u64)
        }
    }

    /// Check for negative feedback streak.
    pub fn negative_streak(&self) -> u32 {
        self.negative_streak_since(None)
    }

    /// Check for negative feedback streak within a time window.
    pub fn negative_streak_in_window(&self, window: Duration) -> u32 {
        self.negative_streak_since(Some(SystemTime::now() - window))
    }

    fn negative_streak_since(&self, cutoff: Option<SystemTime>) -> u32 {
        let mut streak = 0;
        for signal in self.signals.iter().rev() {
            if let Some(cutoff) = cutoff
                && signal.timestamp < cutoff
            {
                break;
            }
            let is_negative = matches!(
                signal.signal_type,
                SignalType::ThumbsRating { positive: false }
                    | SignalType::TextFeedback {
                        sentiment: Sentiment::Negative
                    }
                    | SignalType::Retry { .. }
                    | SignalType::Correction
            );
            if is_negative {
                streak += 1;
            } else if matches!(
                signal.signal_type,
                SignalType::ThumbsRating { positive: true }
                    | SignalType::TaskSuccess
                    | SignalType::Acceptance
            ) {
                break;
            }
        }
        streak
    }

    fn cleanup_old(&mut self) {
        let cutoff = SystemTime::now() - self.max_age;
        while let Some(front) = self.signals.front() {
            if front.timestamp < cutoff {
                self.signals.pop_front();
            } else {
                break;
            }
        }
    }
}

fn signal_type_matches(signal: &SignalType, type_name: &str) -> bool {
    signal.type_name() == type_name
}

// ─── Auto-Tuning Engine ─────────────────────────────────────────────────────

/// Record of a rule execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleExecution {
    pub rule_id: String,
    pub timestamp: SystemTime,
    pub action: EvolutionAction,
    pub previous_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
    pub rolled_back: bool,
}

/// The main auto-tuning engine.
pub struct AutoTuningEngine {
    /// Evolution rules.
    rules: RwLock<Vec<EvolutionRule>>,
    /// Feedback aggregator.
    aggregator: RwLock<FeedbackAggregator>,
    /// Rule execution history.
    executions: RwLock<Vec<RuleExecution>>,
    /// Last trigger time per rule (for cooldown).
    last_triggered: RwLock<HashMap<String, SystemTime>>,
    /// Whether auto-tuning is enabled.
    enabled: RwLock<bool>,
    /// Optional path for persisting aggregator state across restarts.
    storage_path: Option<PathBuf>,
}

impl Default for AutoTuningEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoTuningEngine {
    /// Create a new auto-tuning engine with no rules and an empty aggregator.
    pub fn new() -> Self {
        Self {
            rules: RwLock::new(Vec::new()),
            aggregator: RwLock::new(FeedbackAggregator::new()),
            executions: RwLock::new(Vec::new()),
            last_triggered: RwLock::new(HashMap::new()),
            enabled: RwLock::new(true),
            storage_path: None,
        }
    }

    /// Create a new auto-tuning engine with persistent aggregator storage.
    ///
    /// Loads any previously persisted aggregator state from `path`.
    pub fn with_storage(path: PathBuf) -> Self {
        let aggregator = if path.exists() {
            std::fs::read(&path)
                .ok()
                .and_then(|data| serde_json::from_slice::<FeedbackAggregator>(&data).ok())
                .unwrap_or_default()
        } else {
            FeedbackAggregator::new()
        };

        Self {
            rules: RwLock::new(Vec::new()),
            aggregator: RwLock::new(aggregator),
            executions: RwLock::new(Vec::new()),
            last_triggered: RwLock::new(HashMap::new()),
            enabled: RwLock::new(true),
            storage_path: Some(path),
        }
    }

    /// Add an evolution rule.
    pub fn add_rule(&self, rule: EvolutionRule) {
        self.rules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(rule);
    }

    /// Remove a rule by ID.
    pub fn remove_rule(&self, rule_id: &str) -> bool {
        let mut rules = self.rules.write_or_recover();
        let len_before = rules.len();
        rules.retain(|r| r.id != rule_id);
        rules.len() < len_before
    }

    /// Enable or disable a rule.
    pub fn set_rule_enabled(&self, rule_id: &str, enabled: bool) -> bool {
        if let Some(rule) = self
            .rules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|r| r.id == rule_id)
        {
            rule.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Record a feedback signal.
    pub fn record_feedback(&self, signal: FeedbackSignal) {
        self.aggregator
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .record(signal);
    }

    /// Return the retained feedback signal window for inspection/testing.
    pub fn recent_signals(&self) -> Vec<FeedbackSignal> {
        self.aggregator
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .recent_signals()
    }

    /// Evaluate all rules and return triggered actions.
    pub fn evaluate(&self, config: &RuntimeConfig) -> Vec<(EvolutionRule, EvolutionAction)> {
        self.evaluate_with_patterns(config, None)
    }

    /// Evaluate rules with optional pattern library for drift detection.
    pub fn evaluate_with_patterns(
        &self,
        config: &RuntimeConfig,
        pattern_library: Option<&PatternLibrary>,
    ) -> Vec<(EvolutionRule, EvolutionAction)> {
        if !*self.enabled.read_or_recover() {
            return Vec::new();
        }

        let rules = self.rules.read_or_recover();
        let aggregator = self.aggregator.read_or_recover();
        let last_triggered = self
            .last_triggered
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now();

        let mut triggered = Vec::new();

        for rule in rules.iter() {
            if !rule.enabled {
                continue;
            }

            // Check cooldown
            if let Some(last) = last_triggered.get(&rule.id) {
                if let Ok(elapsed) = now.duration_since(*last) {
                    if elapsed < rule.cooldown {
                        continue;
                    }
                }
            }

            // Evaluate trigger
            if self.evaluate_trigger(&rule.trigger, &aggregator, config, pattern_library) {
                triggered.push((rule.clone(), rule.action.clone()));
            }
        }

        triggered
    }

    /// Execute triggered actions and update config.
    pub fn execute_actions(
        &self,
        config: &mut RuntimeConfig,
        actions: Vec<(EvolutionRule, EvolutionAction)>,
    ) -> Vec<RuleExecution> {
        let mut executions_store = self.executions.write_or_recover();
        let mut last_triggered = self
            .last_triggered
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut result = Vec::new();
        let now = SystemTime::now();

        for (rule, action) in actions {
            // Anti-oscillation: skip if this rule has been rolled back recently.
            let recent_rollbacks = executions_store
                .iter()
                .rev()
                .take(20)
                .filter(|e| e.rule_id == rule.id && e.rolled_back)
                .count();
            if recent_rollbacks >= 2 {
                astra_core::agent_warn!(
                    "tuning",
                    "rule '{}' suppressed — rolled back {} times recently",
                    rule.id,
                    recent_rollbacks
                );
                continue;
            }

            let (previous_value, new_value) = self.apply_action(config, &action);

            let execution = RuleExecution {
                rule_id: rule.id.clone(),
                timestamp: now,
                action: action.clone(),
                previous_value,
                new_value,
                rolled_back: false,
            };

            result.push(execution.clone());
            executions_store.push(execution);
            last_triggered.insert(rule.id, now);
        }

        result
    }

    /// Run one evaluation cycle: evaluate and execute.
    pub fn run_cycle(&self, config: &mut RuntimeConfig) -> Vec<RuleExecution> {
        let actions = self.evaluate(config);
        self.execute_actions(config, actions)
    }

    /// Run one evaluation cycle with pattern drift detection.
    pub fn run_cycle_with_patterns(
        &self,
        config: &mut RuntimeConfig,
        pattern_library: Option<&PatternLibrary>,
    ) -> Vec<RuleExecution> {
        let actions = self.evaluate_with_patterns(config, pattern_library);
        self.execute_actions(config, actions)
    }

    /// Check if any rollback conditions are met and perform rollbacks.
    pub fn check_rollbacks(&self, config: &mut RuntimeConfig) -> Vec<String> {
        let mut rolled_back = Vec::new();
        let aggregator = self.aggregator.read_or_recover();
        let rules = self.rules.read_or_recover();
        let mut executions = self.executions.write_or_recover();

        for execution in executions.iter_mut() {
            if execution.rolled_back {
                continue;
            }

            if let Some(rule) = rules.iter().find(|r| r.id == execution.rule_id) {
                if let Some(ref condition) = rule.rollback_condition {
                    if self.should_rollback(condition, execution, &aggregator) {
                        // Restore previous value
                        if let Some(ref prev) = execution.previous_value {
                            if let EvolutionAction::AdjustConfig { ref path, .. }
                            | EvolutionAction::SetConfig { ref path, .. } = execution.action
                            {
                                apply_config_value(config, path, prev);
                                execution.rolled_back = true;
                                rolled_back.push(execution.rule_id.clone());
                            }
                        }
                    }
                }
            }
        }

        rolled_back
    }

    /// Enable or disable the entire auto-tuning system.
    pub fn set_enabled(&self, enabled: bool) {
        *self.enabled.write_or_recover() = enabled;
    }

    /// Check if auto-tuning is enabled.
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read_or_recover()
    }

    /// Get execution history.
    pub fn get_executions(&self) -> Vec<RuleExecution> {
        self.executions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Get all rules.
    pub fn get_rules(&self) -> Vec<EvolutionRule> {
        self.rules.read_or_recover().clone()
    }

    fn evaluate_trigger(
        &self,
        trigger: &EvolutionTrigger,
        aggregator: &FeedbackAggregator,
        _config: &RuntimeConfig,
        pattern_library: Option<&PatternLibrary>,
    ) -> bool {
        match trigger {
            EvolutionTrigger::LowSuccessRate {
                threshold,
                window_secs,
                min_samples,
            } => {
                let window = Duration::from_secs(*window_secs);
                let signals = aggregator.signals_in_window(window);
                if signals.len() < *min_samples as usize {
                    return false;
                }
                if let Some(rate) = aggregator.success_rate(window) {
                    rate < *threshold
                } else {
                    false
                }
            }

            EvolutionTrigger::HighTokenUsage {
                threshold_tokens,
                window_secs,
                min_samples,
            } => {
                let window = Duration::from_secs(*window_secs);
                let signals = aggregator.signals_in_window(window);
                if signals.len() < *min_samples as usize {
                    return false;
                }
                if let Some(avg) = aggregator.avg_token_usage(window) {
                    avg > *threshold_tokens
                } else {
                    false
                }
            }

            EvolutionTrigger::HighRetryRate {
                threshold,
                window_secs,
                min_samples,
            } => {
                let window = Duration::from_secs(*window_secs);
                let signals = aggregator.signals_in_window(window);
                if signals.len() < *min_samples as usize {
                    return false;
                }
                let retries = aggregator.count_signals(SignalType::NAME_RETRY, window);
                let total = signals.len() as f64;
                (retries as f64 / total) > *threshold
            }

            EvolutionTrigger::NegativeFeedbackStreak { count } => {
                aggregator.negative_streak() >= *count
            }

            EvolutionTrigger::PatternDrift { confidence_drop } => {
                // Check if any pattern has drifted by the threshold amount
                let Some(lib) = pattern_library else {
                    return false;
                };
                // Find maximum drift score across all patterns
                let max_drift = lib.max_drift_score();
                max_drift >= *confidence_drop
            }

            EvolutionTrigger::SignalAccumulation {
                signal_type,
                count,
                window_secs,
            } => {
                let window = Duration::from_secs(*window_secs);
                aggregator.count_signals(signal_type, window) >= *count
            }
        }
    }

    fn should_rollback(
        &self,
        condition: &RollbackCondition,
        execution: &RuleExecution,
        aggregator: &FeedbackAggregator,
    ) -> bool {
        let now = SystemTime::now();

        match condition {
            RollbackCondition::SuccessRateDrops {
                threshold,
                window_secs,
            } => {
                let window = Duration::from_secs(*window_secs);
                // Only evaluate signals after the execution
                if let Some(rate) = aggregator.success_rate(window) {
                    rate < *threshold
                } else {
                    false
                }
            }

            RollbackCondition::NegativeFeedbackIncreases { count, window_secs } => {
                let window = Duration::from_secs(*window_secs);
                aggregator.negative_streak_in_window(window) >= *count
            }

            RollbackCondition::TimeLimit { secs } => {
                if let Ok(elapsed) = now.duration_since(execution.timestamp) {
                    elapsed >= Duration::from_secs(*secs)
                } else {
                    false
                }
            }
        }
    }

    fn apply_action(
        &self,
        config: &mut RuntimeConfig,
        action: &EvolutionAction,
    ) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
        match action {
            EvolutionAction::AdjustConfig {
                path,
                delta,
                min,
                max,
            } => {
                let prev = get_config_value(config, path);
                if let Some(prev_val) = prev.as_ref().and_then(|v| v.as_f64()) {
                    let mut new_val = prev_val + delta;
                    if let Some(min_val) = min {
                        new_val = new_val.max(*min_val);
                    }
                    if let Some(max_val) = max {
                        new_val = new_val.min(*max_val);
                    }
                    let new = serde_json::json!(new_val);
                    apply_config_value(config, path, &new);
                    (prev, Some(new))
                } else {
                    (prev, None)
                }
            }

            EvolutionAction::SetConfig { path, value } => {
                let prev = get_config_value(config, path);
                apply_config_value(config, path, value);
                (prev, Some(value.clone()))
            }

            EvolutionAction::SwitchStrategy { .. } => {
                // Would need strategy registry integration
                (None, None)
            }

            EvolutionAction::EnableExperiment { .. }
            | EvolutionAction::DisableExperiment { .. } => {
                // Would need experiment store integration
                (None, None)
            }

            EvolutionAction::ResetConfig { path } => {
                let prev = get_config_value(config, path);
                let default = RuntimeConfig::default();
                let default_val = get_config_value(&default, path);
                if let Some(ref val) = default_val {
                    apply_config_value(config, path, val);
                }
                (prev, default_val)
            }

            EvolutionAction::Alert { .. } => {
                // Just log, no config change
                (None, None)
            }
        }
    }

    /// Serialize the feedback aggregator state for persistence.
    pub fn save_aggregator(&self) -> Result<Vec<u8>, String> {
        let agg = self.aggregator.read_or_recover();
        serde_json::to_vec_pretty(&*agg).map_err(|e| format!("serialize aggregator: {e}"))
    }

    /// Restore feedback aggregator state from persisted data.
    pub fn load_aggregator(&self, data: &[u8]) -> Result<(), String> {
        let loaded: FeedbackAggregator =
            serde_json::from_slice(data).map_err(|e| format!("deserialize aggregator: {e}"))?;
        *self.aggregator.write_or_recover() = loaded;
        Ok(())
    }

    /// Persist the current aggregator state to disk (if storage path is set).
    ///
    /// Uses atomic write (temp file → rename) to avoid corruption.
    pub fn persist(&self) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("[auto-tuning] failed to create storage directory: {err}");
                return;
            }
        }
        let Ok(data) = self.save_aggregator() else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, data) {
            eprintln!("[auto-tuning] failed to write temp file: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            eprintln!("[auto-tuning] failed to rename temp file: {err}");
        }
    }
}

fn get_config_value(config: &RuntimeConfig, path: &str) -> Option<serde_json::Value> {
    match path {
        "tool_selection.confidence_threshold" => Some(serde_json::json!(
            config.tool_selection.confidence_threshold
        )),
        "tool_selection.max_tools" => Some(serde_json::json!(config.tool_selection.max_tools)),
        "tool_selection.tool_budget_tokens" => {
            Some(serde_json::json!(config.tool_selection.tool_budget_tokens))
        }
        "token_budget.max_prompt_tokens" => {
            Some(serde_json::json!(config.token_budget.max_prompt_tokens))
        }
        "token_budget.system_prompt_reserve" => {
            Some(serde_json::json!(config.token_budget.system_prompt_reserve))
        }
        "token_budget.max_turn_input_tokens" => {
            Some(serde_json::json!(config.token_budget.max_turn_input_tokens))
        }
        "compression.compression_threshold" => {
            Some(serde_json::json!(config.compression.compression_threshold))
        }
        "compression.max_history_tokens" => {
            Some(serde_json::json!(config.compression.max_history_tokens))
        }
        "compression.preserve_recent_turns" => {
            Some(serde_json::json!(config.compression.preserve_recent_turns))
        }
        "memory.retrieval_top_k" => Some(serde_json::json!(config.memory.retrieval_top_k)),
        "memory.min_relevance_score" => Some(serde_json::json!(config.memory.min_relevance_score)),
        "verification.strictness" => Some(serde_json::json!(config.verification.strictness)),
        other => {
            astra_core::agent_warn!("config", "Unknown config path for read: {}", other);
            None
        }
    }
}

fn apply_config_value(config: &mut RuntimeConfig, path: &str, value: &serde_json::Value) {
    match path {
        "tool_selection.confidence_threshold" => {
            if let Some(v) = value.as_f64() {
                config.tool_selection.confidence_threshold = v.clamp(0.0, 1.0);
            }
        }
        "tool_selection.max_tools" => {
            if let Some(v) = value.as_u64() {
                config.tool_selection.max_tools = (v as u32).clamp(1, 256);
            }
        }
        "tool_selection.tool_budget_tokens" => {
            if let Some(v) = value.as_u64() {
                config.tool_selection.tool_budget_tokens = (v as u32).clamp(0, 5000);
            }
        }
        "token_budget.max_prompt_tokens" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.max_prompt_tokens = (v as u32).clamp(1000, 500_000);
            }
        }
        "token_budget.system_prompt_reserve" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.system_prompt_reserve = (v as u32).clamp(500, 100_000);
            }
        }
        "token_budget.max_turn_input_tokens" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.max_turn_input_tokens = (v as u32).clamp(1000, 500_000);
            }
        }
        "compression.compression_threshold" => {
            if let Some(v) = value.as_f64() {
                config.compression.compression_threshold = v.clamp(0.0, 1.0);
            }
        }
        "compression.max_history_tokens" => {
            if let Some(v) = value.as_u64() {
                config.compression.max_history_tokens = (v as u32).min(1_000_000);
            }
        }
        "compression.preserve_recent_turns" => {
            if let Some(v) = value.as_u64() {
                config.compression.preserve_recent_turns = (v as u32).min(100);
            }
        }
        "memory.retrieval_top_k" => {
            if let Some(v) = value.as_u64() {
                config.memory.retrieval_top_k = (v as u32).clamp(1, 100);
            }
        }
        "memory.min_relevance_score" => {
            if let Some(v) = value.as_f64() {
                config.memory.min_relevance_score = v.clamp(0.0, 1.0);
            }
        }
        "verification.strictness" => {
            if let Some(v) = value.as_f64() {
                config.verification.strictness = v.clamp(
                    config.verification.min_strictness,
                    config.verification.max_strictness,
                );
            }
        }
        other => {
            astra_core::agent_warn!("config", "Unknown config path for write: {}", other);
        }
    }
}

// ─── Feedback Persistence ───────────────────────────────────────────────────

/// Path for a profile's feedback state file.
pub fn feedback_path(profile: &str) -> std::path::PathBuf {
    crate::pipeline::persistence::learning_dir().join(format!("{profile}.feedback.json"))
}

/// Save feedback aggregator state atomically.
pub fn save_feedback(profile: &str, engine: &AutoTuningEngine) -> Result<(), String> {
    let path = feedback_path(profile);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let data = engine.save_aggregator()?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Load feedback aggregator state from disk (graceful — returns Ok(false) if missing).
pub fn load_feedback(profile: &str, engine: &AutoTuningEngine) -> Result<bool, String> {
    let path = feedback_path(profile);
    match std::fs::read(&path) {
        Ok(data) => {
            engine.load_aggregator(&data)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("read feedback: {e}")),
    }
}

// ─── Preset Rules ───────────────────────────────────────────────────────────

/// Create default evolution rules.
pub fn default_rules() -> Vec<EvolutionRule> {
    vec![
        // Low success rate → increase confidence threshold
        EvolutionRule::new(
            "low-success-boost-confidence",
            EvolutionTrigger::LowSuccessRate {
                threshold: 0.7,
                window_secs: 3600,
                min_samples: 10,
            },
            EvolutionAction::AdjustConfig {
                path: "tool_selection.confidence_threshold".to_string(),
                delta: 0.05,
                min: Some(0.5),
                max: Some(0.95),
            },
        )
        .with_name("Boost confidence on low success")
        .with_rollback(RollbackCondition::SuccessRateDrops {
            threshold: 0.6,
            window_secs: 1800,
        }),
        // High retry rate → reduce max tools
        EvolutionRule::new(
            "high-retry-reduce-tools",
            EvolutionTrigger::HighRetryRate {
                threshold: 0.3,
                window_secs: 3600,
                min_samples: 10,
            },
            EvolutionAction::AdjustConfig {
                path: "tool_selection.max_tools".to_string(),
                delta: -2.0,
                min: Some(3.0),
                max: Some(15.0),
            },
        )
        .with_name("Reduce tools on high retry rate"),
        // Negative feedback streak → alert
        EvolutionRule::new(
            "negative-streak-alert",
            EvolutionTrigger::NegativeFeedbackStreak { count: 5 },
            EvolutionAction::Alert {
                message: "5 consecutive negative feedback signals detected".to_string(),
                severity: AlertSeverity::Warning,
            },
        )
        .with_name("Alert on negative streak")
        .with_cooldown(Duration::from_secs(7200)),
        // ── Verification surface ──
        // User corrections accumulate → raise review strictness
        EvolutionRule::new(
            "correction-raise-strictness",
            EvolutionTrigger::SignalAccumulation {
                signal_type: SignalType::NAME_CORRECTION.to_string(),
                count: 2,
                window_secs: 3600,
            },
            EvolutionAction::AdjustConfig {
                path: "verification.strictness".to_string(),
                delta: 0.1,
                min: Some(0.2),
                max: Some(0.9),
            },
        )
        .with_name("Raise verification on corrections")
        .with_cooldown(Duration::from_secs(600)),
        // ── Memory-pressure surface ──
        // Tool churn → expand memory retrieval to give agent more context
        EvolutionRule::new(
            "churn-expand-memory",
            EvolutionTrigger::SignalAccumulation {
                signal_type: SignalType::NAME_TOOL_CHURN.to_string(),
                count: 1,
                window_secs: 600,
            },
            EvolutionAction::AdjustConfig {
                path: "memory.retrieval_top_k".to_string(),
                delta: 2.0,
                min: Some(3.0),
                max: Some(15.0),
            },
        )
        .with_name("Expand memory on tool churn")
        .with_cooldown(Duration::from_secs(300)),
        // Focus drift → reset preserve_recent_turns to 1 (aggressive trim)
        EvolutionRule::new(
            "drift-trim-history",
            EvolutionTrigger::SignalAccumulation {
                signal_type: SignalType::NAME_FOCUS_DRIFT.to_string(),
                count: 1,
                window_secs: 600,
            },
            EvolutionAction::SetConfig {
                path: "compression.preserve_recent_turns".to_string(),
                value: serde_json::json!(1),
            },
        )
        .with_name("Trim history on focus drift")
        .with_cooldown(Duration::from_secs(600)),
        // ── Token/context-window surface ──
        // High token usage → reduce turn budget to prevent runaway
        EvolutionRule::new(
            "high-tokens-reduce-budget",
            EvolutionTrigger::HighTokenUsage {
                threshold_tokens: 70_000,
                window_secs: 600,
                min_samples: 1,
            },
            EvolutionAction::AdjustConfig {
                path: "token_budget.max_turn_input_tokens".to_string(),
                delta: -5000.0,
                min: Some(30_000.0),
                max: None,
            },
        )
        .with_name("Reduce token budget on high usage")
        .with_cooldown(Duration::from_secs(300)),
        // High token usage → also lower compression threshold to compress sooner
        EvolutionRule::new(
            "high-tokens-compress-earlier",
            EvolutionTrigger::HighTokenUsage {
                threshold_tokens: 70_000,
                window_secs: 600,
                min_samples: 1,
            },
            EvolutionAction::AdjustConfig {
                path: "compression.compression_threshold".to_string(),
                delta: -0.05,
                min: Some(0.5),
                max: Some(0.95),
            },
        )
        .with_name("Compress earlier on high token usage")
        .with_cooldown(Duration::from_secs(300)),
        // ── Engagement surface ──
        // Rapid follow-ups → reduce history preservation (user is iterating fast)
        EvolutionRule::new(
            "quick-followup-trim-history",
            EvolutionTrigger::SignalAccumulation {
                signal_type: SignalType::NAME_QUICK_FOLLOW_UP.to_string(),
                count: 3,
                window_secs: 600,
            },
            EvolutionAction::AdjustConfig {
                path: "compression.preserve_recent_turns".to_string(),
                delta: -1.0,
                min: Some(1.0),
                max: Some(10.0),
            },
        )
        .with_name("Trim history on rapid follow-ups")
        .with_cooldown(Duration::from_secs(300)),
        // Long pauses → expand memory retrieval (user may need broader context)
        EvolutionRule::new(
            "long-pause-expand-memory",
            EvolutionTrigger::SignalAccumulation {
                signal_type: SignalType::NAME_LONG_PAUSE.to_string(),
                count: 2,
                window_secs: 1800,
            },
            EvolutionAction::AdjustConfig {
                path: "memory.retrieval_top_k".to_string(),
                delta: 2.0,
                min: Some(3.0),
                max: Some(15.0),
            },
        )
        .with_name("Expand memory on long pauses")
        .with_cooldown(Duration::from_secs(600)),
        // ── Pattern drift surface ──
        // Pattern confidence drop → alert + expand exploration
        EvolutionRule::new(
            "pattern-drift-alert",
            EvolutionTrigger::PatternDrift {
                confidence_drop: 0.3,
            },
            EvolutionAction::Alert {
                message: "Pattern confidence drift detected (>0.3 drop)".to_string(),
                severity: AlertSeverity::Warning,
            },
        )
        .with_name("Alert on pattern drift")
        .with_cooldown(Duration::from_secs(1800)),
    ]
}

// ─── Delegation Outcome Tracker ─────────────────────────────────────────────

/// Per-(scenario, pattern) outcome statistics for coordination auto-select.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomeStats {
    pub successes: u32,
    pub failures: u32,
}

impl OutcomeStats {
    /// Success rate as a fraction in [0, 1]. Returns 0.5 when no data.
    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            0.5 // neutral prior
        } else {
            self.successes as f64 / total as f64
        }
    }

    /// Total observations.
    pub fn total(&self) -> u32 {
        self.successes + self.failures
    }
}

/// Tracks delegation outcomes per (scenario, pattern) pair to learn which
/// coordination patterns work best for each scenario.
///
/// Thread-safe via interior `RwLock`. Persistence follows the same atomic
/// write pattern used by `AdaptiveBaselineStore` and `AutoTuningEngine`.
pub struct DelegationOutcomeTracker {
    /// (scenario_key, pattern_name) → outcome stats
    data: RwLock<HashMap<String, OutcomeStats>>,
    storage_path: Option<PathBuf>,
}

impl DelegationOutcomeTracker {
    /// Create an in-memory tracker (no persistence).
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
            storage_path: None,
        }
    }

    /// Create a tracker with persistent storage. Loads existing data on init.
    pub fn with_storage(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        Self {
            data: RwLock::new(data),
            storage_path: Some(path),
        }
    }

    /// Composite key for the data map.
    fn key(scenario: &str, pattern: &str) -> String {
        format!("{scenario}:{pattern}")
    }

    /// Record a delegation outcome.
    pub fn record(&self, scenario: &str, pattern: &str, succeeded: bool) {
        if let Ok(mut map) = self.data.write() {
            let entry = map.entry(Self::key(scenario, pattern)).or_default();
            if succeeded {
                entry.successes += 1;
            } else {
                entry.failures += 1;
            }
        }
    }

    /// Get outcome stats for a specific (scenario, pattern) pair.
    pub fn stats(&self, scenario: &str, pattern: &str) -> Option<OutcomeStats> {
        self.data
            .read()
            .ok()
            .and_then(|map| map.get(&Self::key(scenario, pattern)).cloned())
    }

    /// Find the best pattern for a given scenario based on historical success rates.
    ///
    /// Returns `None` if no data is available or if no pattern has enough observations
    /// (at least `min_observations`).
    pub fn preferred_pattern(&self, scenario: &str, min_observations: u32) -> Option<String> {
        let map = self.data.read().ok()?;
        let prefix = format!("{scenario}:");
        let mut best: Option<(String, f64)> = None;

        for (key, stats) in map.iter() {
            if !key.starts_with(&prefix) || stats.total() < min_observations {
                continue;
            }
            let pattern = &key[prefix.len()..];
            let rate = stats.success_rate();
            if best
                .as_ref()
                .map_or(true, |(_, best_rate)| rate > *best_rate)
            {
                best = Some((pattern.to_string(), rate));
            }
        }
        best.map(|(p, _)| p)
    }

    /// Persist data to storage (atomic write).
    pub fn persist(&self) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("[delegation-outcomes] failed to create storage directory: {err}");
                return;
            }
        }
        let map = match self.data.read() {
            Ok(m) => m,
            Err(_) => return,
        };
        let Ok(data) = serde_json::to_vec_pretty(&*map) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, data) {
            eprintln!("[delegation-outcomes] failed to write temp file: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            eprintln!("[delegation-outcomes] failed to rename temp file: {err}");
        }
    }
}

impl Default for DelegationOutcomeTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feedback_signal_creation() {
        let signal = FeedbackSignal::new(SignalType::TaskSuccess)
            .with_turn("turn-123")
            .with_context("tokens", serde_json::json!(1500));

        assert!(matches!(signal.signal_type, SignalType::TaskSuccess));
        assert_eq!(signal.turn_id, Some("turn-123".to_string()));
        assert_eq!(signal.context.get("tokens"), Some(&serde_json::json!(1500)));
    }

    #[test]
    fn test_feedback_aggregator() {
        let mut agg = FeedbackAggregator::new();

        agg.record(FeedbackSignal::new(SignalType::TaskSuccess));
        agg.record(FeedbackSignal::new(SignalType::TaskSuccess));
        agg.record(FeedbackSignal::new(SignalType::TaskFailure {
            reason: "test".to_string(),
        }));

        let rate = agg.success_rate(Duration::from_secs(60)).unwrap();
        assert!((rate - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_negative_streak() {
        let mut agg = FeedbackAggregator::new();

        agg.record(FeedbackSignal::new(SignalType::TaskSuccess));
        agg.record(FeedbackSignal::new(SignalType::ThumbsRating {
            positive: false,
        }));
        agg.record(FeedbackSignal::new(SignalType::Correction));
        agg.record(FeedbackSignal::new(SignalType::Retry { count: 1 }));

        assert_eq!(agg.negative_streak(), 3);
    }

    #[test]
    fn test_negative_streak_in_window_ignores_older_negatives() {
        let mut agg = FeedbackAggregator::new();

        let mut old_negative = FeedbackSignal::new(SignalType::Correction);
        old_negative.timestamp = SystemTime::now() - Duration::from_secs(120);
        agg.record(old_negative);

        agg.record(FeedbackSignal::new(SignalType::ThumbsRating {
            positive: false,
        }));
        agg.record(FeedbackSignal::new(SignalType::Retry { count: 1 }));

        assert_eq!(agg.negative_streak(), 3);
        assert_eq!(agg.negative_streak_in_window(Duration::from_secs(30)), 2);
    }

    #[test]
    fn test_negative_streak_in_window_stops_at_positive_signal() {
        let mut agg = FeedbackAggregator::new();

        agg.record(FeedbackSignal::new(SignalType::ThumbsRating {
            positive: false,
        }));
        agg.record(FeedbackSignal::new(SignalType::TaskSuccess));
        agg.record(FeedbackSignal::new(SignalType::Correction));

        assert_eq!(agg.negative_streak_in_window(Duration::from_secs(60)), 1);
    }

    #[test]
    fn test_evolution_rule_creation() {
        let rule = EvolutionRule::new(
            "test-rule",
            EvolutionTrigger::LowSuccessRate {
                threshold: 0.7,
                window_secs: 3600,
                min_samples: 10,
            },
            EvolutionAction::AdjustConfig {
                path: "tool_selection.confidence_threshold".to_string(),
                delta: 0.05,
                min: None,
                max: None,
            },
        )
        .with_name("Test Rule")
        .with_cooldown(Duration::from_secs(1800));

        assert_eq!(rule.id, "test-rule");
        assert_eq!(rule.name, "Test Rule");
        assert_eq!(rule.cooldown, Duration::from_secs(1800));
    }

    #[test]
    fn test_auto_tuning_engine() {
        let engine = AutoTuningEngine::new();

        // Add a rule that triggers immediately
        engine.add_rule(EvolutionRule::new(
            "immediate-trigger",
            EvolutionTrigger::NegativeFeedbackStreak { count: 2 },
            EvolutionAction::Alert {
                message: "Test alert".to_string(),
                severity: AlertSeverity::Info,
            },
        ));

        // Record negative feedback
        engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
            positive: false,
        }));
        engine.record_feedback(FeedbackSignal::new(SignalType::Correction));

        let config = RuntimeConfig::default();
        let triggered = engine.evaluate(&config);

        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0].0.id, "immediate-trigger");
    }

    #[test]
    fn test_config_adjustment() {
        let engine = AutoTuningEngine::new();
        let mut config = RuntimeConfig::default();
        let initial_threshold = config.tool_selection.confidence_threshold;

        let action = EvolutionAction::AdjustConfig {
            path: "tool_selection.confidence_threshold".to_string(),
            delta: 0.1,
            min: Some(0.5),
            max: Some(0.95),
        };

        let rule = EvolutionRule::new(
            "test",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            action.clone(),
        );
        let executions = engine.execute_actions(&mut config, vec![(rule, action)]);

        // Verify execution was recorded
        assert_eq!(executions.len(), 1);

        // Check the threshold was adjusted
        // Since initial is 0.3, delta is 0.1, but min is 0.5, result should be clamped to 0.5
        let expected = (initial_threshold + 0.1).max(0.5).min(0.95);
        assert!(
            (config.tool_selection.confidence_threshold - expected).abs() < 0.001,
            "Expected {}, got {}, initial was {}",
            expected,
            config.tool_selection.confidence_threshold,
            initial_threshold
        );
    }

    #[test]
    fn test_default_rules() {
        let rules = default_rules();
        assert_eq!(rules.len(), 11);
        assert!(rules.iter().any(|r| r.id == "low-success-boost-confidence"));
        assert!(rules.iter().any(|r| r.id == "correction-raise-strictness"));
        assert!(rules.iter().any(|r| r.id == "churn-expand-memory"));
        assert!(rules.iter().any(|r| r.id == "drift-trim-history"));
        assert!(rules.iter().any(|r| r.id == "high-tokens-reduce-budget"));
        assert!(rules.iter().any(|r| r.id == "high-tokens-compress-earlier"));
        assert!(rules.iter().any(|r| r.id == "quick-followup-trim-history"));
        assert!(rules.iter().any(|r| r.id == "long-pause-expand-memory"));
        assert!(rules.iter().any(|r| r.id == "pattern-drift-alert"));
    }

    #[test]
    fn signal_type_name_constants_are_consistent() {
        // Verify type_name() returns the constant for every variant.
        assert_eq!(
            SignalType::Retry { count: 1 }.type_name(),
            SignalType::NAME_RETRY
        );
        assert_eq!(
            SignalType::Correction.type_name(),
            SignalType::NAME_CORRECTION
        );
        assert_eq!(
            SignalType::QuickFollowUp { delay_ms: 100 }.type_name(),
            SignalType::NAME_QUICK_FOLLOW_UP
        );
        assert_eq!(
            SignalType::LongPause { delay_ms: 60000 }.type_name(),
            SignalType::NAME_LONG_PAUSE
        );
        assert_eq!(
            SignalType::StarRating { stars: 4 }.type_name(),
            SignalType::NAME_STAR_RATING
        );
        assert_eq!(
            SignalType::TextFeedback {
                sentiment: Sentiment::Positive
            }
            .type_name(),
            SignalType::NAME_TEXT_FEEDBACK
        );
        assert_eq!(
            SignalType::FocusDrift.type_name(),
            SignalType::NAME_FOCUS_DRIFT
        );
    }

    #[test]
    fn quick_followup_rule_triggers_on_accumulated_signals() {
        let engine = AutoTuningEngine::new();
        for rule in default_rules() {
            engine.add_rule(rule);
        }
        let config = RuntimeConfig::default();

        // Record 3 quick follow-up signals within the window
        for _ in 0..3 {
            engine.record_feedback(FeedbackSignal::new(SignalType::QuickFollowUp {
                delay_ms: 500,
            }));
        }

        let result = engine.evaluate(&config);
        assert!(
            result
                .iter()
                .any(|(r, _)| r.id == "quick-followup-trim-history"),
            "Quick follow-up rule should trigger on 3+ signals"
        );
    }

    #[test]
    fn long_pause_rule_triggers_on_accumulated_signals() {
        let engine = AutoTuningEngine::new();
        for rule in default_rules() {
            engine.add_rule(rule);
        }
        let config = RuntimeConfig::default();

        // Record 2 long pause signals within the window
        for _ in 0..2 {
            engine.record_feedback(FeedbackSignal::new(SignalType::LongPause {
                delay_ms: 120_000,
            }));
        }

        let result = engine.evaluate(&config);
        assert!(
            result
                .iter()
                .any(|(r, _)| r.id == "long-pause-expand-memory"),
            "Long pause rule should trigger on 2+ signals"
        );
    }

    #[test]
    fn test_config_paths_get_set_round_trip() {
        let mut config = RuntimeConfig::default();

        // Verification strictness
        let orig = get_config_value(&config, "verification.strictness");
        assert!(orig.is_some());
        assert!((orig.unwrap().as_f64().unwrap() - 0.5).abs() < 0.001);

        apply_config_value(
            &mut config,
            "verification.strictness",
            &serde_json::json!(0.8),
        );
        assert!((config.verification.strictness - 0.8).abs() < 0.001);

        // Memory retrieval_top_k
        apply_config_value(
            &mut config,
            "memory.retrieval_top_k",
            &serde_json::json!(10),
        );
        assert_eq!(config.memory.retrieval_top_k, 10);

        // Compression preserve_recent_turns
        apply_config_value(
            &mut config,
            "compression.preserve_recent_turns",
            &serde_json::json!(1),
        );
        assert_eq!(config.compression.preserve_recent_turns, 1);

        // Token budget
        apply_config_value(
            &mut config,
            "token_budget.max_turn_input_tokens",
            &serde_json::json!(50_000),
        );
        assert_eq!(config.token_budget.max_turn_input_tokens, 50_000);
    }

    #[test]
    fn test_verification_strictness_clamped_by_config() {
        let mut config = RuntimeConfig::default();
        config.verification.min_strictness = 0.3;
        config.verification.max_strictness = 0.7;

        // Try to set above max — should clamp
        apply_config_value(
            &mut config,
            "verification.strictness",
            &serde_json::json!(0.95),
        );
        assert!((config.verification.strictness - 0.7).abs() < 0.001);

        // Try to set below min — should clamp
        apply_config_value(
            &mut config,
            "verification.strictness",
            &serde_json::json!(0.1),
        );
        assert!((config.verification.strictness - 0.3).abs() < 0.001);
    }

    #[test]
    fn test_signal_type_matching() {
        assert!(signal_type_matches(
            &SignalType::Retry { count: 2 },
            "retry"
        ));
        assert!(signal_type_matches(
            &SignalType::TaskSuccess,
            "task_success"
        ));
        assert!(!signal_type_matches(&SignalType::TaskSuccess, "retry"));
    }

    #[test]
    fn test_enable_disable() {
        let engine = AutoTuningEngine::new();
        engine.add_rule(EvolutionRule::new(
            "test",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                message: "test".to_string(),
                severity: AlertSeverity::Info,
            },
        ));

        engine.record_feedback(FeedbackSignal::new(SignalType::Correction));

        // Should trigger when enabled
        let config = RuntimeConfig::default();
        assert_eq!(engine.evaluate(&config).len(), 1);

        // Disable and check
        engine.set_enabled(false);
        assert_eq!(engine.evaluate(&config).len(), 0);
    }

    #[test]
    fn aggregator_round_trip_serialization() {
        let engine = AutoTuningEngine::new();
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskSuccess));
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskFailure {
            reason: "test".into(),
        }));
        engine.record_feedback(FeedbackSignal::new(SignalType::HighTokenUsage {
            tokens: 5000,
            threshold: 4000,
        }));

        let data = engine.save_aggregator().expect("save should succeed");

        let engine2 = AutoTuningEngine::new();
        engine2.load_aggregator(&data).expect("load should succeed");

        // Verify signals survived round-trip
        let config = RuntimeConfig::default();
        engine2.add_rule(EvolutionRule::new(
            "low-success",
            EvolutionTrigger::LowSuccessRate {
                threshold: 0.8,
                window_secs: 3600,
                min_samples: 1,
            },
            EvolutionAction::Alert {
                message: "test".into(),
                severity: AlertSeverity::Info,
            },
        ));
        let triggered = engine2.evaluate(&config);
        // 1 success / 2 total = 0.5 < 0.8 threshold
        assert!(
            !triggered.is_empty(),
            "restored signals should trigger rule"
        );
    }

    #[test]
    fn save_load_feedback_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let profile = "test-persist";
        let path = dir.path().join(format!("{profile}.feedback.json"));

        let engine = AutoTuningEngine::new();
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskSuccess));
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskSuccess));

        // Save directly to temp path (bypass learning_dir)
        let data = engine.save_aggregator().unwrap();
        std::fs::write(&path, &data).unwrap();

        // Load into new engine
        let engine2 = AutoTuningEngine::new();
        let loaded = std::fs::read(&path).unwrap();
        engine2.load_aggregator(&loaded).unwrap();

        let config = RuntimeConfig::default();
        engine2.add_rule(EvolutionRule::new(
            "acc",
            EvolutionTrigger::SignalAccumulation {
                signal_type: "task_success".into(),
                count: 2,
                window_secs: 3600,
            },
            EvolutionAction::Alert {
                message: "ok".into(),
                severity: AlertSeverity::Info,
            },
        ));
        assert!(
            !engine2.evaluate(&config).is_empty(),
            "loaded 2 success signals should trigger"
        );
    }

    // ── Feedback Persistence Tests ──

    #[test]
    fn tuning_engine_persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuning.json");

        // Create engine, record signals, persist.
        let engine = AutoTuningEngine::with_storage(path.clone());
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskSuccess));
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskFailure {
            reason: "oops".into(),
        }));
        engine.persist();

        assert!(path.exists(), "persist should create file");

        // Load a new engine from same path — should recover signals.
        let engine2 = AutoTuningEngine::with_storage(path);
        let data = engine2.save_aggregator().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
        // The aggregator should have at least some data from the 2 signals.
        assert!(!data.is_empty());
        assert!(json.is_object());
    }

    #[test]
    fn tuning_engine_persist_no_storage_is_noop() {
        let engine = AutoTuningEngine::new();
        engine.record_feedback(FeedbackSignal::new(SignalType::TaskSuccess));
        engine.persist(); // Should not panic or error.
    }

    // ── Tool Health Signal Type Tests ──

    #[test]
    fn tool_deprioritized_signal_type_name() {
        let signal = FeedbackSignal::new(SignalType::ToolDeprioritized {
            tool_name: "bash".into(),
        });
        assert_eq!(signal.signal_type.type_name(), "tool_deprioritized");
    }

    #[test]
    fn tool_rehabilitated_signal_type_name() {
        let signal = FeedbackSignal::new(SignalType::ToolRehabilitated {
            tool_name: "bash".into(),
        });
        assert_eq!(signal.signal_type.type_name(), "tool_rehabilitated");
    }

    #[test]
    fn tool_signals_serializable() {
        let dep = SignalType::ToolDeprioritized {
            tool_name: "view".into(),
        };
        let json = serde_json::to_string(&dep).unwrap();
        assert!(json.contains("view"));
        let rehab = SignalType::ToolRehabilitated {
            tool_name: "edit".into(),
        };
        let json = serde_json::to_string(&rehab).unwrap();
        assert!(json.contains("edit"));
    }

    // ── Delegation Outcome Tracker Tests ──

    #[test]
    fn outcome_tracker_record_and_stats() {
        let tracker = DelegationOutcomeTracker::new();
        tracker.record("code_review", "adversarial", true);
        tracker.record("code_review", "adversarial", true);
        tracker.record("code_review", "adversarial", false);

        let stats = tracker.stats("code_review", "adversarial").unwrap();
        assert_eq!(stats.successes, 2);
        assert_eq!(stats.failures, 1);
        assert!((stats.success_rate() - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn outcome_tracker_preferred_pattern() {
        let tracker = DelegationOutcomeTracker::new();
        // fan_out: 4 successes, 1 failure (80%)
        for _ in 0..4 {
            tracker.record("exploration", "fan_out", true);
        }
        tracker.record("exploration", "fan_out", false);
        // sequential: 2 successes, 3 failures (40%)
        for _ in 0..2 {
            tracker.record("exploration", "sequential", true);
        }
        for _ in 0..3 {
            tracker.record("exploration", "sequential", false);
        }

        let pref = tracker.preferred_pattern("exploration", 3).unwrap();
        assert_eq!(pref, "fan_out");
    }

    #[test]
    fn outcome_tracker_preferred_pattern_min_observations() {
        let tracker = DelegationOutcomeTracker::new();
        tracker.record("debugging", "sequential", true);
        tracker.record("debugging", "sequential", true);
        // Only 2 observations, min is 3
        assert!(tracker.preferred_pattern("debugging", 3).is_none());
    }

    #[test]
    fn outcome_tracker_persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcomes.json");

        let tracker = DelegationOutcomeTracker::with_storage(path.clone());
        tracker.record("testing", "fan_out", true);
        tracker.record("testing", "fan_out", true);
        tracker.record("testing", "fan_out", false);
        tracker.persist();

        assert!(path.exists());

        // Reload from disk.
        let tracker2 = DelegationOutcomeTracker::with_storage(path);
        let stats = tracker2.stats("testing", "fan_out").unwrap();
        assert_eq!(stats.successes, 2);
        assert_eq!(stats.failures, 1);
    }

    #[test]
    fn outcome_stats_neutral_prior() {
        let stats = OutcomeStats::default();
        assert_eq!(stats.total(), 0);
        assert!((stats.success_rate() - 0.5).abs() < f64::EPSILON);
    }
}
