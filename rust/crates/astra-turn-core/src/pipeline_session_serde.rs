//! Helpers for restoring a `PipelineSession` from checkpoint state.
//!
//! The write side is `PipelineSession::snapshot_full_state()` +
//! `serde_json::to_value(snapshot)` (called from
//! `agentic_loop_finalization::persist_heavy_checkpoint`). The read side is
//! a raw `serde_json::Value` loaded from `HeavyCheckpoint.pipeline_state`.
//!
//! This module provides the resume-time glue: it parses that Value back into
//! a `PipelineSessionSnapshot`, distinguishes missing / corrupt / valid
//! payloads, and logs so operators can diagnose warm-start failures.

use serde_json::Value;

use crate::pipeline_config::PipelineConfig;
use crate::pipeline_session::{PipelineSession, PipelineSessionSnapshot};
use crate::pipeline_stats::PipelineStats;

/// Outcome of attempting to parse `HeavyCheckpoint.pipeline_state`.
#[derive(Debug)]
pub enum RestoreOutcome {
    /// Field was null or missing — legitimate for checkpoints written before
    /// pipeline_state existed. Start with a fresh session.
    Missing,
    /// Payload parsed successfully. Caller should pass this into
    /// `PipelineSession::from_snapshot`.
    Restored(PipelineSessionSnapshot),
    /// Payload is present but schema-incompatible. Caller SHOULD log so
    /// operators can diagnose warm-start loss, then start fresh.
    Corrupt(serde_json::Error),
}

/// Parse a `pipeline_state` JSON value into a snapshot.
///
/// This is the inverse of `serde_json::to_value(session.snapshot_full_state())`
/// that `agentic_loop_finalization` writes. The three-outcome enum forces
/// callers to distinguish legitimate absence from corruption.
pub fn parse_pipeline_state(value: Option<&Value>) -> RestoreOutcome {
    let Some(value) = value else {
        return RestoreOutcome::Missing;
    };
    if value.is_null() {
        return RestoreOutcome::Missing;
    }
    // An empty object is treated as "missing field" — older checkpoints that
    // serialized a default struct can manifest as `{}` for truly-empty state.
    if value.as_object().is_some_and(|o| o.is_empty()) {
        return RestoreOutcome::Missing;
    }
    match serde_json::from_value::<PipelineSessionSnapshot>(value.clone()) {
        Ok(snapshot) => RestoreOutcome::Restored(snapshot),
        Err(err) => RestoreOutcome::Corrupt(err),
    }
}

/// Convenience: build a `PipelineSession` from an optional checkpoint value.
///
/// - Missing / null → fresh `PipelineSession::new(config)`
/// - Corrupt payload → fresh session + `tracing::warn!` (operators see why
///   warm-start data was lost; they can investigate the on-disk blob)
/// - Valid snapshot → `PipelineSession::from_snapshot(config, snapshot)`
///   (retains stats / latches / emergent / recovery-escalation counters)
///
/// This is the canonical way to construct a `PipelineSession` on server
/// resume. Use it at EVERY site that currently calls `PipelineSession::new`
/// with a checkpoint value available — otherwise warm-start silently
/// regresses to cold.
#[must_use]
pub fn restore_or_new(config: PipelineConfig, checkpoint_value: Option<&Value>) -> PipelineSession {
    match parse_pipeline_state(checkpoint_value) {
        RestoreOutcome::Missing => PipelineSession::new(config),
        RestoreOutcome::Restored(snapshot) => {
            tracing::debug!(
                turns = snapshot.stats.turns_executed,
                cache_breaks = snapshot.stats.cache_breaks.len(),
                "pipeline session restored from checkpoint (warm-start)"
            );
            PipelineSession::from_snapshot(config, snapshot)
        }
        RestoreOutcome::Corrupt(err) => {
            tracing::warn!(
                error = %err,
                "pipeline_state checkpoint is corrupt; starting fresh. \
                 Warm-start data lost — cache/feedback history reset to defaults"
            );
            PipelineSession::new(config)
        }
    }
}

/// Serialize PipelineStats to JSON bytes for persistence (standalone).
pub fn serialize_stats(stats: &PipelineStats) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(stats)
}

/// Deserialize PipelineStats from JSON bytes (standalone).
pub fn deserialize_stats(bytes: &[u8]) -> Result<PipelineStats, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_feedback::ContextFeedback;
    use crate::recovery_state::RecoveryState;
    use crate::section_types::CacheScope;
    use crate::session_latches::SessionLatches;

    #[test]
    fn parse_none_is_missing() {
        assert!(matches!(parse_pipeline_state(None), RestoreOutcome::Missing));
    }

    #[test]
    fn parse_null_is_missing() {
        assert!(matches!(
            parse_pipeline_state(Some(&Value::Null)),
            RestoreOutcome::Missing
        ));
    }

    #[test]
    fn parse_empty_object_is_missing() {
        let v = serde_json::json!({});
        assert!(matches!(parse_pipeline_state(Some(&v)), RestoreOutcome::Missing));
    }

    #[test]
    fn parse_valid_snapshot_restores() {
        // Write-side round-trip: snapshot_full_state → to_value → parse
        let mut stats = PipelineStats::default();
        let feedback = ContextFeedback::from_usage(0, 900, 100, 500, false);
        stats.record("claude-sonnet-4-6", "repl", &feedback);

        let mut latches = SessionLatches::default();
        latches.latch_cache_scope(CacheScope::Global, 1);

        let mut recovery = RecoveryState::default();
        recovery.record_output_escalation();

        let session = PipelineSession::from_snapshot(
            PipelineConfig::default(),
            PipelineSessionSnapshot {
                stats: stats.clone(),
                latches: latches.clone(),
                recovery,
                emergent: Default::default(),
            },
        );
        let value = serde_json::to_value(session.snapshot_full_state()).unwrap();

        match parse_pipeline_state(Some(&value)) {
            RestoreOutcome::Restored(snap) => {
                assert_eq!(snap.stats.turns_executed, 1);
                assert_eq!(snap.latches.cache_scope, Some(CacheScope::Global));
                // output-escalation preserved across restore
                assert_eq!(snap.recovery.max_output_escalation_count, 1);
            }
            other => panic!("expected Restored, got {other:?}"),
        }
    }

    #[test]
    fn parse_corrupt_returns_corrupt_variant() {
        // Right general shape, wrong types — forces serde error
        let corrupt = serde_json::json!({
            "stats": "not-a-struct",
            "latches": {},
            "recovery": {},
            "emergent": {}
        });
        let out = parse_pipeline_state(Some(&corrupt));
        assert!(
            matches!(out, RestoreOutcome::Corrupt(_)),
            "expected Corrupt, got {out:?}"
        );
    }

    #[test]
    fn restore_or_new_missing_yields_fresh_session() {
        let sess = restore_or_new(PipelineConfig::default(), None);
        assert_eq!(sess.stats.turns_executed, 0);
    }

    #[test]
    fn restore_or_new_corrupt_falls_back_to_fresh() {
        let corrupt = serde_json::json!({"stats": "bad"});
        let sess = restore_or_new(PipelineConfig::default(), Some(&corrupt));
        assert_eq!(
            sess.stats.turns_executed, 0,
            "corrupt payload must fall back to fresh, not panic"
        );
    }

    #[test]
    fn restore_or_new_valid_preserves_warm_state() {
        let mut stats = PipelineStats::default();
        let feedback = ContextFeedback::from_usage(0, 900, 100, 500, false);
        stats.record("m", "q", &feedback);
        stats.record_compaction(2000);

        let original = PipelineSession::from_snapshot(
            PipelineConfig::default(),
            PipelineSessionSnapshot {
                stats: stats.clone(),
                latches: SessionLatches::default(),
                recovery: RecoveryState::default(),
                emergent: Default::default(),
            },
        );
        let value = serde_json::to_value(original.snapshot_full_state()).unwrap();

        let restored = restore_or_new(PipelineConfig::default(), Some(&value));
        assert_eq!(
            restored.stats.turns_executed, 1,
            "warm-start must preserve stats across restore"
        );
        assert_eq!(restored.stats.compact_events.len(), 1);
    }

    // ── Stats standalone serialization ──

    #[test]
    fn roundtrip_stats() {
        let mut stats = PipelineStats::default();
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        stats.record("m", "q", &feedback);
        let bytes = serialize_stats(&stats).unwrap();
        let restored = deserialize_stats(&bytes).unwrap();
        assert_eq!(restored.turns_executed, 1);
        assert!((restored.avg_cache_hit_ratio - stats.avg_cache_hit_ratio).abs() < 1e-9);
    }

    #[test]
    fn deserialize_stats_rejects_garbage() {
        let result = deserialize_stats(b"not json");
        assert!(result.is_err());
    }
}
