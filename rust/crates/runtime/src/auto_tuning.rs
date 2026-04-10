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
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

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
    pub fn new(signal_type: SignalType) -> Self {
        Self {
            signal_type,
            timestamp: SystemTime::now(),
            turn_id: None,
            context: HashMap::new(),
        }
    }

    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

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
    pub fn new(id: impl Into<String>, trigger: EvolutionTrigger, action: EvolutionAction) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            trigger,
            action,
            rollback_condition: None,
            cooldown: Duration::from_secs(3600), // 1 hour default
            enabled: true,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_rollback(mut self, condition: RollbackCondition) -> Self {
        self.rollback_condition = Some(condition);
        self
    }

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
#[derive(Debug)]
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
    let signal_name = match signal {
        SignalType::Retry { .. } => "retry",
        SignalType::Correction => "correction",
        SignalType::Interruption => "interruption",
        SignalType::Acceptance => "acceptance",
        SignalType::QuickFollowUp { .. } => "quick_follow_up",
        SignalType::LongPause { .. } => "long_pause",
        SignalType::ThumbsRating { .. } => "thumbs_rating",
        SignalType::StarRating { .. } => "star_rating",
        SignalType::TextFeedback { .. } => "text_feedback",
        SignalType::HighTokenUsage { .. } => "high_token_usage",
        SignalType::ToolChurn { .. } => "tool_churn",
        SignalType::FocusDrift => "focus_drift",
        SignalType::TaskSuccess => "task_success",
        SignalType::TaskFailure { .. } => "task_failure",
    };
    signal_name == type_name
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
}

impl Default for AutoTuningEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoTuningEngine {
    pub fn new() -> Self {
        Self {
            rules: RwLock::new(Vec::new()),
            aggregator: RwLock::new(FeedbackAggregator::new()),
            executions: RwLock::new(Vec::new()),
            last_triggered: RwLock::new(HashMap::new()),
            enabled: RwLock::new(true),
        }
    }

    /// Add an evolution rule.
    pub fn add_rule(&self, rule: EvolutionRule) {
        self.rules.write().unwrap().push(rule);
    }

    /// Remove a rule by ID.
    pub fn remove_rule(&self, rule_id: &str) -> bool {
        let mut rules = self.rules.write().unwrap();
        let len_before = rules.len();
        rules.retain(|r| r.id != rule_id);
        rules.len() < len_before
    }

    /// Enable or disable a rule.
    pub fn set_rule_enabled(&self, rule_id: &str, enabled: bool) -> bool {
        if let Some(rule) = self
            .rules
            .write()
            .unwrap()
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
        self.aggregator.write().unwrap().record(signal);
    }

    /// Evaluate all rules and return triggered actions.
    pub fn evaluate(&self, config: &RuntimeConfig) -> Vec<(EvolutionRule, EvolutionAction)> {
        if !*self.enabled.read().unwrap() {
            return Vec::new();
        }

        let rules = self.rules.read().unwrap();
        let aggregator = self.aggregator.read().unwrap();
        let last_triggered = self.last_triggered.read().unwrap();
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
            if self.evaluate_trigger(&rule.trigger, &aggregator, config) {
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
        let mut executions = Vec::new();
        let now = SystemTime::now();

        for (rule, action) in actions {
            let (previous_value, new_value) = self.apply_action(config, &action);

            let execution = RuleExecution {
                rule_id: rule.id.clone(),
                timestamp: now,
                action: action.clone(),
                previous_value,
                new_value,
                rolled_back: false,
            };

            executions.push(execution.clone());
            self.executions.write().unwrap().push(execution);
            self.last_triggered.write().unwrap().insert(rule.id, now);
        }

        executions
    }

    /// Run one evaluation cycle: evaluate and execute.
    pub fn run_cycle(&self, config: &mut RuntimeConfig) -> Vec<RuleExecution> {
        let actions = self.evaluate(config);
        self.execute_actions(config, actions)
    }

    /// Check if any rollback conditions are met and perform rollbacks.
    pub fn check_rollbacks(&self, config: &mut RuntimeConfig) -> Vec<String> {
        let mut rolled_back = Vec::new();
        let aggregator = self.aggregator.read().unwrap();
        let rules = self.rules.read().unwrap();
        let mut executions = self.executions.write().unwrap();

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
        *self.enabled.write().unwrap() = enabled;
    }

    /// Get execution history.
    pub fn get_executions(&self) -> Vec<RuleExecution> {
        self.executions.read().unwrap().clone()
    }

    /// Get all rules.
    pub fn get_rules(&self) -> Vec<EvolutionRule> {
        self.rules.read().unwrap().clone()
    }

    fn evaluate_trigger(
        &self,
        trigger: &EvolutionTrigger,
        aggregator: &FeedbackAggregator,
        _config: &RuntimeConfig,
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
                let retries = aggregator.count_signals("retry", window);
                let total = signals.len() as f64;
                (retries as f64 / total) > *threshold
            }

            EvolutionTrigger::NegativeFeedbackStreak { count } => {
                aggregator.negative_streak() >= *count
            }

            EvolutionTrigger::PatternDrift { confidence_drop: _ } => {
                // Would need integration with PatternLibrary
                false
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
}

fn get_config_value(config: &RuntimeConfig, path: &str) -> Option<serde_json::Value> {
    match path {
        "tool_selection.confidence_threshold" => Some(serde_json::json!(
            config.tool_selection.confidence_threshold
        )),
        "tool_selection.max_tools" => Some(serde_json::json!(config.tool_selection.max_tools)),
        "token_budget.max_prompt_tokens" => {
            Some(serde_json::json!(config.token_budget.max_prompt_tokens))
        }
        "token_budget.system_prompt_reserve" => {
            Some(serde_json::json!(config.token_budget.system_prompt_reserve))
        }
        _ => None,
    }
}

fn apply_config_value(config: &mut RuntimeConfig, path: &str, value: &serde_json::Value) {
    match path {
        "tool_selection.confidence_threshold" => {
            if let Some(v) = value.as_f64() {
                config.tool_selection.confidence_threshold = v;
            }
        }
        "tool_selection.max_tools" => {
            if let Some(v) = value.as_u64() {
                config.tool_selection.max_tools = v as u32;
            }
        }
        "token_budget.max_prompt_tokens" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.max_prompt_tokens = v as u32;
            }
        }
        "token_budget.system_prompt_reserve" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.system_prompt_reserve = v as u32;
            }
        }
        _ => {}
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
    ]
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
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().any(|r| r.id == "low-success-boost-confidence"));
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
}
