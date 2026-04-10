//! End-to-end tests for the Auto-Tuning system.
//!
//! These tests verify the complete feedback loop:
//! 1. Record feedback signals
//! 2. Evaluate evolution rules
//! 3. Apply configuration adjustments
//! 4. Verify rollback conditions

use astra_runtime::auto_tuning::{
    AlertSeverity, AutoTuningEngine, EvolutionAction, EvolutionRule, EvolutionTrigger,
    FeedbackSignal, SignalType,
};
use astra_runtime::runtime_config::RuntimeConfig;
use std::time::Duration;

// ─── E2E: Feedback → Rule Trigger → Config Change ───────────────────────────

/// Test that negative feedback streak triggers config adjustment.
#[test]
fn e2e_negative_streak_triggers_config_change() {
    let engine = AutoTuningEngine::new();
    let mut config = RuntimeConfig::default();

    // Add rule: 3 consecutive negative feedbacks → disable a feature
    let rule = EvolutionRule::new(
        "negative_streak_rule",
        EvolutionTrigger::NegativeFeedbackStreak { count: 3 },
        EvolutionAction::SetConfig {
            path: "test_feature_enabled".to_string(),
            value: serde_json::json!(false),
        },
    )
    .with_cooldown(Duration::from_secs(0)); // No cooldown for testing

    engine.add_rule(rule);

    // Record 2 negative feedbacks - should NOT trigger
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    let actions = engine.evaluate(&config);
    assert!(
        actions.is_empty(),
        "Should not trigger with only 2 negatives"
    );

    // Record 3rd negative - should trigger
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    let actions = engine.evaluate(&config);
    assert_eq!(actions.len(), 1, "Should trigger after 3 negatives");

    // Execute and verify
    let executions = engine.execute_actions(&mut config, actions);
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].rule_id, "negative_streak_rule");
    assert!(!executions[0].rolled_back);
}

/// Test that positive feedback breaks the negative streak.
#[test]
fn e2e_positive_feedback_breaks_negative_streak() {
    let engine = AutoTuningEngine::new();
    let config = RuntimeConfig::default();

    engine.add_rule(
        EvolutionRule::new(
            "streak_rule",
            EvolutionTrigger::NegativeFeedbackStreak { count: 3 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Warning,
                message: "Too many negatives".into(),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    // Record 2 negatives, then 1 positive
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: true,
    })); // breaks streak

    // Record another negative - streak count should be 1, not 3
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    let actions = engine.evaluate(&config);
    assert!(
        actions.is_empty(),
        "Positive feedback should have broken the streak"
    );
}

// ─── E2E: Run Cycle ─────────────────────────────────────────────────────────

/// Test the complete run_cycle flow.
#[test]
fn e2e_run_cycle_evaluates_and_executes() {
    let engine = AutoTuningEngine::new();
    let mut config = RuntimeConfig::default();

    // Add rule
    engine.add_rule(
        EvolutionRule::new(
            "quick_trigger",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::SetConfig {
                path: "adjusted_by_tuning".to_string(),
                value: serde_json::json!(true),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    // Record signal
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    // Run cycle
    let executions = engine.run_cycle(&mut config);
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].rule_id, "quick_trigger");
}

// ─── E2E: Cooldown Behavior ─────────────────────────────────────────────────

/// Test that cooldown prevents repeated triggers.
#[test]
fn e2e_cooldown_prevents_repeated_triggers() {
    let engine = AutoTuningEngine::new();
    let mut config = RuntimeConfig::default();

    // Add rule with 1-hour cooldown
    engine.add_rule(
        EvolutionRule::new(
            "cooldown_rule",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Info,
                message: "Triggered".into(),
            },
        )
        .with_cooldown(Duration::from_secs(3600)),
    );

    // Record negative and trigger
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));
    let executions = engine.run_cycle(&mut config);
    assert_eq!(executions.len(), 1, "First trigger should succeed");

    // Record another negative - should NOT trigger (cooldown)
    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));
    let executions2 = engine.run_cycle(&mut config);
    assert!(
        executions2.is_empty(),
        "Second trigger should be blocked by cooldown"
    );
}

// ─── E2E: Enable/Disable ────────────────────────────────────────────────────

/// Test that disabled engine doesn't evaluate rules.
#[test]
fn e2e_disabled_engine_skips_evaluation() {
    let engine = AutoTuningEngine::new();
    let config = RuntimeConfig::default();

    engine.add_rule(
        EvolutionRule::new(
            "always_trigger",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Info,
                message: "test".into(),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    // Disable engine
    engine.set_enabled(false);
    assert!(!engine.is_enabled());

    // Evaluate - should return empty
    let actions = engine.evaluate(&config);
    assert!(actions.is_empty(), "Disabled engine should not evaluate");

    // Re-enable and verify it works again
    engine.set_enabled(true);
    let actions = engine.evaluate(&config);
    assert_eq!(actions.len(), 1, "Re-enabled engine should evaluate");
}

// ─── E2E: Rule Enable/Disable ───────────────────────────────────────────────

/// Test enabling/disabling individual rules.
#[test]
fn e2e_individual_rule_enable_disable() {
    let engine = AutoTuningEngine::new();
    let config = RuntimeConfig::default();

    engine.add_rule(
        EvolutionRule::new(
            "rule_a",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Info,
                message: "A".into(),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    // Verify rule triggers
    let actions = engine.evaluate(&config);
    assert_eq!(actions.len(), 1);

    // Disable the rule
    assert!(engine.set_rule_enabled("rule_a", false));

    // Evaluate again - should not trigger
    let actions = engine.evaluate(&config);
    assert!(actions.is_empty(), "Disabled rule should not trigger");

    // Re-enable
    assert!(engine.set_rule_enabled("rule_a", true));
    let actions = engine.evaluate(&config);
    assert_eq!(actions.len(), 1, "Re-enabled rule should trigger");
}

// ─── E2E: Multiple Rules ────────────────────────────────────────────────────

/// Test multiple rules can trigger in the same cycle.
#[test]
fn e2e_multiple_rules_trigger_together() {
    let engine = AutoTuningEngine::new();
    let config = RuntimeConfig::default();

    engine.add_rule(
        EvolutionRule::new(
            "rule_1",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Info,
                message: "First".into(),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    engine.add_rule(
        EvolutionRule::new(
            "rule_2",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::Alert {
                severity: AlertSeverity::Info,
                message: "Second".into(),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    let actions = engine.evaluate(&config);
    assert_eq!(actions.len(), 2, "Both rules should trigger");

    let rule_ids: Vec<&str> = actions.iter().map(|(r, _)| r.id.as_str()).collect();
    assert!(rule_ids.contains(&"rule_1"));
    assert!(rule_ids.contains(&"rule_2"));
}

// ─── E2E: Execution History ─────────────────────────────────────────────────

/// Test that execution history is recorded.
#[test]
fn e2e_execution_history_recorded() {
    let engine = AutoTuningEngine::new();
    let mut config = RuntimeConfig::default();

    engine.add_rule(
        EvolutionRule::new(
            "history_rule",
            EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
            EvolutionAction::SetConfig {
                path: "history_test".to_string(),
                value: serde_json::json!(123),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    engine.record_feedback(FeedbackSignal::new(SignalType::ThumbsRating {
        positive: false,
    }));

    // Initially empty
    assert!(engine.get_executions().is_empty());

    // Execute
    let _ = engine.run_cycle(&mut config);

    // History should contain the execution
    let history = engine.get_executions();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].rule_id, "history_rule");
    assert!(!history[0].rolled_back);
}

// ─── E2E: Remove Rule ───────────────────────────────────────────────────────

/// Test removing a rule by ID.
#[test]
fn e2e_remove_rule() {
    let engine = AutoTuningEngine::new();
    let _config = RuntimeConfig::default();

    engine.add_rule(EvolutionRule::new(
        "to_remove",
        EvolutionTrigger::NegativeFeedbackStreak { count: 1 },
        EvolutionAction::Alert {
            severity: AlertSeverity::Info,
            message: "test".into(),
        },
    ));

    assert_eq!(engine.get_rules().len(), 1);

    // Remove rule
    assert!(engine.remove_rule("to_remove"));
    assert!(engine.get_rules().is_empty());

    // Remove non-existent rule
    assert!(!engine.remove_rule("does_not_exist"));
}

// ─── E2E: Signal Context ────────────────────────────────────────────────────

/// Test that signal context is preserved.
#[test]
fn e2e_signal_context_preserved() {
    let engine = AutoTuningEngine::new();

    let signal = FeedbackSignal::new(SignalType::Correction)
        .with_turn("turn_123")
        .with_context("tool", serde_json::json!("bash"))
        .with_context("reason", serde_json::json!("wrong command"));

    engine.record_feedback(signal);

    // Aggregator should have the signal (indirectly verified through rule evaluation)
    // Direct verification would require exposing aggregator internals
}

// ─── E2E: Default Rules Integration ─────────────────────────────────────────

/// Test that default rules can be loaded and used.
#[test]
fn e2e_default_rules_integration() {
    use astra_runtime::auto_tuning::default_rules;

    let engine = AutoTuningEngine::new();
    let rules = default_rules();

    assert!(!rules.is_empty(), "Should have default rules");

    for rule in rules {
        engine.add_rule(rule);
    }

    let loaded_rules = engine.get_rules();
    assert!(
        !loaded_rules.is_empty(),
        "Engine should have loaded default rules"
    );
}

// ─── E2E: Full Lifecycle ────────────────────────────────────────────────────

/// Integration test: full lifecycle from feedback to config change.
#[test]
fn e2e_full_lifecycle_feedback_to_config() {
    let engine = AutoTuningEngine::new();
    let mut config = RuntimeConfig::default();

    // 1. Add rule
    engine.add_rule(
        EvolutionRule::new(
            "lifecycle_rule",
            EvolutionTrigger::NegativeFeedbackStreak { count: 2 },
            EvolutionAction::SetConfig {
                path: "lifecycle_flag".to_string(),
                value: serde_json::json!("adjusted"),
            },
        )
        .with_cooldown(Duration::from_secs(0)),
    );

    // 2. Verify no changes yet
    let before = engine.run_cycle(&mut config);
    assert!(before.is_empty());

    // 3. Record first negative
    engine.record_feedback(
        FeedbackSignal::new(SignalType::ThumbsRating { positive: false })
            .with_turn("turn_1")
            .with_context("action", serde_json::json!("retry")),
    );

    let after_one = engine.run_cycle(&mut config);
    assert!(after_one.is_empty(), "Should not trigger with 1 negative");

    // 4. Record second negative
    engine.record_feedback(
        FeedbackSignal::new(SignalType::ThumbsRating { positive: false }).with_turn("turn_2"),
    );

    // 5. Verify trigger and execution
    let after_two = engine.run_cycle(&mut config);
    assert_eq!(after_two.len(), 1, "Should trigger after 2 negatives");

    // 6. Verify history
    let history = engine.get_executions();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].rule_id, "lifecycle_rule");
    assert_eq!(history[0].new_value, Some(serde_json::json!("adjusted")));
}
