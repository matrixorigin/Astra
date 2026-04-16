//! Tactical adapter — evaluates step-level signals and produces bounded
//! within-turn mutations. The dampener prevents over-reaction by enforcing
//! minimum intervals between same-type actions and maximum actions per turn.

use crate::liquid_step_signals::AdaptationTrigger;
use std::collections::HashMap;

/// Bounded action the tactical adapter can take within a single turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TacticalAction {
    /// Increase verification strictness (e.g., ask model to double-check).
    IncreaseVerification { reason: String },
    /// Suggest switching to a different tool for the stalled one.
    SuggestToolSwitch { from_tool: String, reason: String },
    /// Warn about token budget pressure.
    TokenBudgetWarning { used: u64, budget: u64 },
    /// Reduce concurrency or slow down (after high-latency detection).
    ThrottleHint { reason: String },
    /// No action needed — system is operating normally.
    NoOp,
}

/// Configuration for the adaptation dampener.
#[derive(Debug, Clone)]
pub struct DampenerConfig {
    /// Minimum number of tool calls between firing the same action type.
    pub min_calls_between_same_type: u32,
    /// Maximum total tactical actions per turn.
    pub max_actions_per_turn: u32,
    /// If cumulative drift from baseline config exceeds this fraction, freeze.
    pub drift_freeze_threshold: f64,
}

impl Default for DampenerConfig {
    fn default() -> Self {
        Self {
            min_calls_between_same_type: 3,
            max_actions_per_turn: 2,
            drift_freeze_threshold: 0.3,
        }
    }
}

/// Tracks dampening state to prevent over-adaptation.
#[derive(Debug)]
pub struct AdaptationDampener {
    config: DampenerConfig,
    /// action_type_key → step_number when last fired.
    last_fired: HashMap<String, u32>,
    current_step: u32,
    actions_this_turn: u32,
    cumulative_drift: f64,
}

impl AdaptationDampener {
    pub fn new(config: DampenerConfig) -> Self {
        Self {
            config,
            last_fired: HashMap::new(),
            current_step: 0,
            actions_this_turn: 0,
            cumulative_drift: 0.0,
        }
    }

    /// Check if the action is allowed by the dampener.
    pub fn allow(&mut self, action: &TacticalAction) -> bool {
        if matches!(action, TacticalAction::NoOp) {
            return true;
        }

        // Max actions per turn.
        if self.actions_this_turn >= self.config.max_actions_per_turn {
            return false;
        }

        // Drift freeze.
        if self.cumulative_drift >= self.config.drift_freeze_threshold {
            return false;
        }

        // Min calls between same type.
        let key = action_type_key(action);
        if let Some(&last_step) = self.last_fired.get(&key) {
            if self.current_step.saturating_sub(last_step) < self.config.min_calls_between_same_type
            {
                return false;
            }
        }

        true
    }

    /// Record that an action was applied.
    pub fn record_action(&mut self, action: &TacticalAction, drift_delta: f64) {
        if matches!(action, TacticalAction::NoOp) {
            return;
        }
        let key = action_type_key(action);
        self.last_fired.insert(key, self.current_step);
        self.actions_this_turn += 1;
        self.cumulative_drift += drift_delta;
    }

    /// Advance the step counter (call after each tool execution).
    pub fn advance_step(&mut self) {
        self.current_step += 1;
    }

    /// Reset for a new turn.
    pub fn reset_turn(&mut self) {
        self.actions_this_turn = 0;
        // Decay drift gradually so the system can recover from drift-freeze.
        // 10% decay per turn prevents permanent lockout while still penalizing drift.
        if self.cumulative_drift > 0.0 {
            self.cumulative_drift *= 0.9;
            // Snap to zero when negligible to avoid floating-point drift.
            if self.cumulative_drift < 0.01 {
                self.cumulative_drift = 0.0;
            }
        }
    }

    pub fn actions_this_turn(&self) -> u32 {
        self.actions_this_turn
    }

    pub fn cumulative_drift(&self) -> f64 {
        self.cumulative_drift
    }
}

/// The tactical adapter: evaluates triggers → produces dampened actions.
#[derive(Debug)]
pub struct TacticalAdapter {
    dampener: AdaptationDampener,
}

impl TacticalAdapter {
    pub fn new(dampener_config: DampenerConfig) -> Self {
        Self {
            dampener: AdaptationDampener::new(dampener_config),
        }
    }

    /// Evaluate a set of triggers and return the actions to apply.
    /// Actions are filtered through the dampener.
    pub fn evaluate(&mut self, triggers: &[AdaptationTrigger]) -> Vec<TacticalAction> {
        let mut actions = Vec::new();

        for trigger in triggers {
            let candidate = match trigger {
                AdaptationTrigger::ErrorStreak { count, last_tool } => {
                    TacticalAction::IncreaseVerification {
                        reason: format!("{count} consecutive errors (last: {last_tool})"),
                    }
                }
                AdaptationTrigger::ToolStall {
                    tool_name, calls, ..
                } => TacticalAction::SuggestToolSwitch {
                    from_tool: tool_name.clone(),
                    reason: format!("{calls} failed calls to {tool_name}"),
                },
                AdaptationTrigger::TokenPressure { used, budget, .. } => {
                    TacticalAction::TokenBudgetWarning {
                        used: *used,
                        budget: *budget,
                    }
                }
                AdaptationTrigger::HighLatency {
                    latency_ms,
                    threshold_ms,
                } => TacticalAction::ThrottleHint {
                    reason: format!(
                        "latency {}ms exceeds {}ms threshold",
                        latency_ms, threshold_ms
                    ),
                },
                AdaptationTrigger::Nominal => TacticalAction::NoOp,
            };

            if self.dampener.allow(&candidate) {
                let drift = action_drift_delta(&candidate);
                self.dampener.record_action(&candidate, drift);
                actions.push(candidate);
            }
        }

        actions
    }

    /// Advance the dampener step counter.
    pub fn advance_step(&mut self) {
        self.dampener.advance_step();
    }

    /// Reset for a new turn.
    pub fn reset_turn(&mut self) {
        self.dampener.reset_turn();
    }

    pub fn dampener(&self) -> &AdaptationDampener {
        &self.dampener
    }
}

fn action_type_key(action: &TacticalAction) -> String {
    match action {
        TacticalAction::IncreaseVerification { .. } => "verify".into(),
        TacticalAction::SuggestToolSwitch { from_tool, .. } => format!("switch:{from_tool}"),
        TacticalAction::TokenBudgetWarning { .. } => "token_warn".into(),
        TacticalAction::ThrottleHint { .. } => "throttle".into(),
        TacticalAction::NoOp => "noop".into(),
    }
}

fn action_drift_delta(action: &TacticalAction) -> f64 {
    match action {
        TacticalAction::IncreaseVerification { .. } => 0.05,
        TacticalAction::SuggestToolSwitch { .. } => 0.10,
        TacticalAction::TokenBudgetWarning { .. } => 0.02,
        TacticalAction::ThrottleHint { .. } => 0.03,
        TacticalAction::NoOp => 0.0,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_adapter() -> TacticalAdapter {
        TacticalAdapter::new(DampenerConfig::default())
    }

    #[test]
    fn nominal_triggers_produce_noop() {
        let mut adapter = make_adapter();
        let actions = adapter.evaluate(&[AdaptationTrigger::Nominal]);
        assert_eq!(actions, vec![TacticalAction::NoOp]);
    }

    #[test]
    fn error_streak_triggers_verification() {
        let mut adapter = make_adapter();
        let actions = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            TacticalAction::IncreaseVerification { .. }
        ));
    }

    #[test]
    fn tool_stall_triggers_switch() {
        let mut adapter = make_adapter();
        let actions = adapter.evaluate(&[AdaptationTrigger::ToolStall {
            tool_name: "bash".into(),
            calls: 4,
        }]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            TacticalAction::SuggestToolSwitch { from_tool, .. } if from_tool == "bash"
        ));
    }

    #[test]
    fn dampener_blocks_same_type_within_interval() {
        let mut adapter = make_adapter();

        // First ErrorStreak → allowed.
        let a1 = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);
        assert_eq!(a1.len(), 1);

        // Immediately again without advancing steps → blocked.
        let a2 = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 4,
            last_tool: "bash".into(),
        }]);
        assert!(a2.is_empty(), "should be dampened");
    }

    #[test]
    fn dampener_allows_after_enough_steps() {
        let mut adapter = make_adapter();

        adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);

        // Advance 3 steps (matches min_calls_between_same_type).
        for _ in 0..3 {
            adapter.advance_step();
        }

        let a2 = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);
        assert_eq!(a2.len(), 1, "should be allowed after enough steps");
    }

    #[test]
    fn max_actions_per_turn_enforced() {
        let mut adapter = TacticalAdapter::new(DampenerConfig {
            max_actions_per_turn: 2,
            min_calls_between_same_type: 0,
            drift_freeze_threshold: 1.0,
        });

        // Action 1.
        let a1 = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);
        assert_eq!(a1.len(), 1);

        // Action 2.
        adapter.advance_step();
        let a2 = adapter.evaluate(&[AdaptationTrigger::ToolStall {
            tool_name: "view".into(),
            calls: 4,
        }]);
        assert_eq!(a2.len(), 1);

        // Action 3 → blocked by max.
        adapter.advance_step();
        let a3 = adapter.evaluate(&[AdaptationTrigger::TokenPressure {
            used: 80000,
            budget: 100000,
            fraction: 0.8,
        }]);
        assert!(a3.is_empty(), "should hit max actions per turn");
    }

    #[test]
    fn drift_freeze_halts_adaptation() {
        let mut adapter = TacticalAdapter::new(DampenerConfig {
            max_actions_per_turn: 100,
            min_calls_between_same_type: 0,
            drift_freeze_threshold: 0.08, // Below the 0.10 from SuggestToolSwitch
        });

        // SuggestToolSwitch has drift 0.10.
        adapter.evaluate(&[AdaptationTrigger::ToolStall {
            tool_name: "bash".into(),
            calls: 4,
        }]);
        assert!((adapter.dampener().cumulative_drift() - 0.10).abs() < 0.01);

        // Cumulative drift (0.10) > threshold (0.08) → frozen.
        adapter.advance_step();
        let a2 = adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "view".into(),
        }]);
        assert!(a2.is_empty(), "should be frozen due to drift");
    }

    #[test]
    fn reset_turn_allows_new_actions() {
        let mut adapter = TacticalAdapter::new(DampenerConfig {
            max_actions_per_turn: 1,
            min_calls_between_same_type: 0,
            drift_freeze_threshold: 1.0,
        });

        adapter.evaluate(&[AdaptationTrigger::ErrorStreak {
            count: 3,
            last_tool: "bash".into(),
        }]);
        assert_eq!(adapter.dampener().actions_this_turn(), 1);

        adapter.reset_turn();
        assert_eq!(adapter.dampener().actions_this_turn(), 0);

        // New turn — should allow again.
        let a = adapter.evaluate(&[AdaptationTrigger::TokenPressure {
            used: 80000,
            budget: 100000,
            fraction: 0.8,
        }]);
        assert_eq!(a.len(), 1);
    }
}
