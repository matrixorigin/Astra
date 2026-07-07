//! Phase 13 TDD: observability, cascade responder, audit trail, session facts.

use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::context_feedback::ContextFeedback;
use astra_turn_core::context_pipeline::PipelineExplain;
use astra_turn_core::context_pressure::ContextPressure;
use astra_turn_core::pipeline_config::PipelineConfig;
use astra_turn_core::pipeline_session::PipelineSession;
use astra_turn_core::pipeline_stats::PipelineStats;

// ── 13.1: TurnTraceCollector pipeline integration ───────────────────────────

#[test]
fn pipeline_explain_serializes_to_trace_compatible_json() {
    let explain = PipelineExplain {
        phase_timings: vec![
            astra_turn_core::context_pipeline::PipelinePhaseTiming {
                phase: "plan".into(),
                elapsed_micros: 150,
            },
            astra_turn_core::context_pipeline::PipelinePhaseTiming {
                phase: "bind".into(),
                elapsed_micros: 80,
            },
        ],
        pressure: ContextPressure {
            value: 0.65,
            raw: 0.55,
        },
        compact_tier: CompactionTier::TrimSchemas,
        skipped_optimizations: 2,
    };

    let json = serde_json::to_value(&explain).unwrap();
    assert_eq!(json["compact_tier"], "trim_schemas");
    assert_eq!(json["skipped_optimizations"], 2);
    assert!(json["pressure"]["value"].as_f64().unwrap() > 0.6);
    assert_eq!(json["phase_timings"].as_array().unwrap().len(), 2);
}

// ── 13.5: Cascade responder ─────────────────────────────────────────────────

#[test]
fn cascade_detection_suppresses_clearing_on_next_turn() {
    let mut sess = PipelineSession::new(PipelineConfig::default());

    // Simulate compaction cascade: 2+ events in 3 turns
    sess.stats.turns_executed = 5;
    sess.stats.record_compaction(3000);
    sess.stats.turns_executed = 6;
    sess.stats.record_compaction(2000);

    assert!(sess.stats.has_compaction_cascade());

    // The adaptive limits should suppress tool_result_clearing
    let limits = sess.cascade_aware_limits(200_000);
    assert!(
        !limits.allow_tool_result_clearing,
        "cascade should suppress clearing to break the loop"
    );
}

#[test]
fn no_cascade_allows_normal_clearing() {
    let mut sess = PipelineSession::new(PipelineConfig::default());
    sess.stats.turns_executed = 10;
    // Only 1 compaction event — no cascade
    sess.stats.record_compaction(1000);

    assert!(!sess.stats.has_compaction_cascade());

    let limits = sess.cascade_aware_limits(200_000);
    assert!(
        limits.allow_tool_result_clearing,
        "no cascade should allow normal clearing"
    );
}

#[test]
fn cascade_responder_emits_alert() {
    use astra_turn_core::recovery_state::RecoveryState;
    use astra_turn_core::trace_alert::evaluate_alerts;

    let mut stats = PipelineStats {
        turns_executed: 5,
        ..Default::default()
    };
    stats.record_compaction(3000);
    stats.turns_executed = 6;
    stats.record_compaction(2000);

    let feedback = ContextFeedback::from_usage(0, 800, 200, 300, false);
    let recovery = RecoveryState::default();
    let alerts = evaluate_alerts(7, &feedback, &stats, &recovery);

    assert!(
        alerts.iter().any(|a| a.rule == "compaction_cascade"),
        "cascade should produce an alert: {:?}",
        alerts
    );
}

// ── 13.6: Compaction audit trail ────────────────────────────────────────────

#[test]
fn pipeline_session_collects_compaction_audits() {
    let mut sess = PipelineSession::new(PipelineConfig::default());

    sess.record_compaction_audit("tool_result_clearing", 5, 2400);
    sess.record_compaction_audit("schema_prune", 3, 800);

    let audits = sess.drain_pending_audits();
    assert_eq!(audits.len(), 2);
    assert_eq!(
        audits[0].compaction_strategy.as_deref(),
        Some("tool_result_clearing")
    );
    assert_eq!(audits[0].tokens_freed, Some(2400));
    assert_eq!(
        audits[1].compaction_strategy.as_deref(),
        Some("schema_prune")
    );
}

#[test]
fn drain_clears_pending_audits() {
    let mut sess = PipelineSession::new(PipelineConfig::default());
    sess.record_compaction_audit("round_dropping", 6, 5000);

    let first = sess.drain_pending_audits();
    assert_eq!(first.len(), 1);

    let second = sess.drain_pending_audits();
    assert!(second.is_empty());
}

// ── 13.7: SessionFacts pipeline extension ───────────────────────────────────

#[test]
fn session_facts_pipeline_fields_from_stats() {
    use astra_turn_core::pipeline_session::PipelineSessionMetrics;

    let mut stats = PipelineStats::default();
    for i in 1..=5 {
        let fb = ContextFeedback::from_usage(0, 800, 200, 300 + i * 50, false);
        stats.record("model", "repl", &fb);
    }
    stats.record_compaction(1000);
    stats.record_compaction(2000);

    let metrics = PipelineSessionMetrics::from_stats(&stats);

    assert!(metrics.avg_cache_hit_ratio > 0.5);
    assert_eq!(metrics.total_compactions, 2);
    assert_eq!(metrics.turns_executed, 5);
}
