//! Phase 12 TDD: persistence, journal events, emergent checkpoint, cloud sync.
//!
//! Tests written first (red), then implementation makes them green.

use astra_turn_core::context_feedback::ContextFeedback;
use astra_turn_core::emergent_context::{DiscoveredSkill, EmergentItem};
use astra_turn_core::pipeline_config::PipelineConfig;
use astra_turn_core::pipeline_session::PipelineSession;
use astra_turn_core::pipeline_session_serde::{
    deserialize_session_state, serialize_session_state,
};
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::section_types::CacheScope;
use astra_turn_core::session_latches::SessionLatches;

// ── 12.2: EmergentContext in checkpoints ────────────────────────────────────

#[test]
fn emergent_context_survives_serialize_roundtrip() {
    let mut sess = PipelineSession::new(PipelineConfig::default());
    sess.push_emergent_skill("debug", "error detected", 1);
    sess.push_emergent_memory("User prefers verbose output.", 0.9, 1);

    let state = sess.snapshot_full_state();
    let json = serde_json::to_value(&state).unwrap();
    let restored: astra_turn_core::pipeline_session::PipelineSessionSnapshot =
        serde_json::from_value(json).unwrap();

    assert_eq!(restored.emergent.discovered_skills.len(), 1);
    assert_eq!(
        restored.emergent.discovered_skills[0].value.skill_name,
        "debug"
    );
    assert_eq!(restored.emergent.prefetched_memory.len(), 1);
}

#[test]
fn emergent_context_restored_into_session() {
    let mut sess = PipelineSession::new(PipelineConfig::default());
    sess.push_emergent_skill("review", "code change", 5);

    let state = sess.snapshot_full_state();
    let json = serde_json::to_value(&state).unwrap();
    let restored: astra_turn_core::pipeline_session::PipelineSessionSnapshot =
        serde_json::from_value(json).unwrap();

    let sess2 = PipelineSession::from_snapshot(PipelineConfig::default(), restored);
    assert!(!sess2.emergent.is_empty());
    assert_eq!(sess2.emergent.discovered_skills[0].value.skill_name, "review");
}

// ── 12.4: Journal events ────────────────────────────────────────────────────

#[test]
fn pipeline_feedback_event_captures_cache_metrics() {
    use astra_turn_core::pipeline_journal::{PipelineJournalEvent, PipelineEventKind};

    let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
    let event = PipelineJournalEvent::from_feedback(3, "claude-sonnet-4-6", &feedback);

    assert_eq!(event.kind, PipelineEventKind::Feedback);
    assert_eq!(event.turn, 3);
    assert!((event.cache_hit_ratio.unwrap() - 0.8).abs() < 1e-9);
    assert_eq!(event.completion_tokens, Some(500));
}

#[test]
fn pipeline_alert_event_captures_rule_and_severity() {
    use astra_turn_core::pipeline_journal::{PipelineJournalEvent, PipelineEventKind};
    use astra_turn_core::trace_alert::{AlertSeverity, TraceAlert};

    let alert = TraceAlert {
        severity: AlertSeverity::Error,
        rule: "recovery_loop".into(),
        message: "3 consecutive PTL errors".into(),
        turn: 7,
    };
    let event = PipelineJournalEvent::from_alert(&alert);

    assert_eq!(event.kind, PipelineEventKind::Alert);
    assert_eq!(event.turn, 7);
    assert_eq!(event.alert_rule.as_deref(), Some("recovery_loop"));
    assert_eq!(event.alert_severity.as_deref(), Some("Error"));
}

#[test]
fn compaction_audit_event_captures_what_was_dropped() {
    use astra_turn_core::pipeline_journal::{PipelineJournalEvent, PipelineEventKind};

    let event = PipelineJournalEvent::compaction_audit(
        5,
        "tool_result_clearing",
        12,
        3400,
    );

    assert_eq!(event.kind, PipelineEventKind::CompactionAudit);
    assert_eq!(event.turn, 5);
    assert_eq!(event.compaction_strategy.as_deref(), Some("tool_result_clearing"));
    assert_eq!(event.items_affected, Some(12));
    assert_eq!(event.tokens_freed, Some(3400));
}

#[test]
fn pipeline_events_serialize_to_journal_compatible_json() {
    use astra_turn_core::pipeline_journal::{PipelineJournalEvent, PipelineEventKind};

    let feedback = ContextFeedback::from_usage(0, 900, 100, 300, false);
    let event = PipelineJournalEvent::from_feedback(1, "model", &feedback);
    let json = serde_json::to_value(&event).unwrap();

    assert_eq!(json["kind"], "Feedback");
    assert_eq!(json["turn"], 1);
    assert!(json["cache_hit_ratio"].as_f64().unwrap() > 0.8);
}

// ── 12.5: Cloud sync compatibility ─────────────────────────────────────────

#[test]
fn pipeline_events_round_trip_through_json() {
    use astra_turn_core::pipeline_journal::{PipelineJournalEvent, PipelineEventKind};

    let event = PipelineJournalEvent::compaction_audit(10, "round_dropping", 6, 8000);
    let json = serde_json::to_string(&event).unwrap();
    let restored: PipelineJournalEvent = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.kind, PipelineEventKind::CompactionAudit);
    assert_eq!(restored.turn, 10);
    assert_eq!(restored.tokens_freed, Some(8000));
}

// ── Full lifecycle: snapshot + journal ───────────────────────────────────────

#[test]
fn full_session_snapshot_includes_all_state() {
    let mut sess = PipelineSession::new(PipelineConfig::default());

    // Simulate 3 turns
    for i in 1..=3 {
        let feedback = ContextFeedback::from_usage(0, 800, 200, 300 + i * 50, false);
        sess.record_feedback("model", "repl", feedback, None);
    }
    sess.push_emergent_skill("test-skill", "trigger", 3);
    sess.latch_cache_scope(CacheScope::Global, 1);
    sess.record_ptl_error();

    let snapshot = sess.snapshot_full_state();

    assert_eq!(snapshot.stats.turns_executed, 3);
    assert!(snapshot.stats.avg_cache_hit_ratio > 0.5);
    assert_eq!(snapshot.latches.cache_scope, Some(CacheScope::Global));
    assert_eq!(snapshot.recovery.consecutive_ptl_errors, 1);
    assert_eq!(snapshot.emergent.discovered_skills.len(), 1);
}
