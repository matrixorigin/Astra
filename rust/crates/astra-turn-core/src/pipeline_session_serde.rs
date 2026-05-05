//! Serialization support for PipelineSession persistence.
//!
//! Enables warm-start: save pipeline state at checkpoint, reload on resume.
//! The envelope format (`PipelineStateEnvelope`) is versioned so schema
//! evolution can be handled gracefully at deserialization time.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;
use crate::session_latches::SessionLatches;

/// Versioned envelope for checkpoint persistence.
/// All pipeline state that should survive session suspend/resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PipelineStateEnvelope {
    version: u32,
    stats: PipelineStats,
    latches: SessionLatches,
    recovery: RecoveryState,
}

const CURRENT_VERSION: u32 = 1;

/// Serialize the pipeline session's cross-turn state into a JSON Value
/// suitable for storage in `HeavyCheckpoint.pipeline_state`.
pub fn serialize_session_state(
    stats: &PipelineStats,
    latches: &SessionLatches,
    recovery: &RecoveryState,
) -> Result<Value, serde_json::Error> {
    let envelope = PipelineStateEnvelope {
        version: CURRENT_VERSION,
        stats: stats.clone(),
        latches: latches.clone(),
        recovery: *recovery,
    };
    serde_json::to_value(&envelope)
}

/// Restored pipeline state from a checkpoint.
#[derive(Debug, Clone)]
pub struct RestoredPipelineState {
    pub stats: PipelineStats,
    pub latches: SessionLatches,
    pub recovery: RecoveryState,
}

/// Deserialize pipeline state from a JSON Value (from `HeavyCheckpoint.pipeline_state`).
///
/// Fallible variant that distinguishes three cases:
///   - `Ok(None)`  — value is null or an empty object (legitimate: old
///     checkpoint written before pipeline_state was added).
///   - `Ok(Some(state))` — parsed successfully.
///   - `Err(e)`    — payload is present but corrupt / schema-incompatible.
///     Callers that care about observability (runtime, cloud restore)
///     must log the error — a silent drop hides warm-start failures.
///
/// On successful parse, transient recovery fields (`consecutive_ptl_errors`,
/// `consecutive_same_errors`, `has_attempted_reactive_compact`) are cleared
/// because PTL errors are in-flight state that doesn't carry across session
/// boundaries. Escalation counters are preserved for reserve widening.
pub fn deserialize_session_state_fallible(
    value: &Value,
) -> Result<Option<RestoredPipelineState>, serde_json::Error> {
    if value.is_null() {
        return Ok(None);
    }
    // Empty object is treated as "field missing" — old checkpoints that never
    // populated pipeline_state show up this way in the persisted JSON.
    if value.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(None);
    }
    let envelope: PipelineStateEnvelope = serde_json::from_value(value.clone())?;
    // Version gate: if future versions need migration, handle here.
    // For now, version 1 is the only version.
    if envelope.version > CURRENT_VERSION {
        // Future version — serde ignored unknown fields during deserialization,
        // so we may have lost fidelity. Log once rather than silently pretending
        // the restore was clean.
        tracing::warn!(
            checkpoint_version = envelope.version,
            supported_version = CURRENT_VERSION,
            "pipeline_state checkpoint is from a newer version; unknown fields dropped"
        );
    }

    let mut recovery = envelope.recovery;
    recovery.consecutive_ptl_errors = 0;
    recovery.consecutive_same_errors = 0;
    recovery.has_attempted_reactive_compact = false;

    Ok(Some(RestoredPipelineState {
        stats: envelope.stats,
        latches: envelope.latches,
        recovery,
    }))
}

/// Back-compat wrapper around [`deserialize_session_state_fallible`] that
/// collapses the corrupt-payload case into `None`.
///
/// **Prefer the fallible variant in new code.** This wrapper is kept for
/// the handful of callers that can't meaningfully act on a parse error
/// (e.g., fire-and-forget telemetry); when they upgrade, delete this.
///
/// On corrupt input this still logs via `tracing::warn!` so operators
/// aren't left with zero signal — the old version was `.ok()?` with no log.
pub fn deserialize_session_state(value: &Value) -> Option<RestoredPipelineState> {
    match deserialize_session_state_fallible(value) {
        Ok(opt) => opt,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "pipeline_state checkpoint is corrupt; starting fresh. \
                 Warm-start data lost — cache/feedback history reset to defaults"
            );
            None
        }
    }
}

/// Serialize PipelineStats to JSON bytes for persistence (legacy API).
pub fn serialize_stats(stats: &PipelineStats) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(stats)
}

/// Deserialize PipelineStats from JSON bytes (legacy API).
pub fn deserialize_stats(bytes: &[u8]) -> Result<PipelineStats, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_feedback::ContextFeedback;
    use crate::section_types::CacheScope;

    #[test]
    fn roundtrip_empty_stats() {
        let stats = PipelineStats::default();
        let bytes = serialize_stats(&stats).unwrap();
        let restored = deserialize_stats(&bytes).unwrap();
        assert_eq!(restored.turns_executed, 0);
        assert_eq!(restored.avg_cache_hit_ratio, 0.0);
    }

    #[test]
    fn roundtrip_populated_stats() {
        let mut stats = PipelineStats::default();
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        stats.record("claude-sonnet-4-6", "repl", &feedback);
        stats.record_compaction(2000);

        let bytes = serialize_stats(&stats).unwrap();
        let restored = deserialize_stats(&bytes).unwrap();

        assert_eq!(restored.turns_executed, 1);
        assert!((restored.avg_cache_hit_ratio - stats.avg_cache_hit_ratio).abs() < 1e-9);
        assert_eq!(restored.compact_events.len(), 1);
        assert_eq!(restored.cache_breaks.len(), 0);
    }

    #[test]
    fn deserialize_gracefully_fails_on_garbage() {
        let result = deserialize_stats(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_preserves_response_token_estimates() {
        let mut stats = PipelineStats::default();
        for i in 1..=20 {
            let feedback = ContextFeedback::from_usage(0, 0, 0, i * 100, false);
            stats.record("model-a", "api", &feedback);
        }

        let bytes = serialize_stats(&stats).unwrap();
        let restored = deserialize_stats(&bytes).unwrap();

        let original_reserve =
            stats
                .response_token_estimates
                .reserve_for("model-a", "api", &RecoveryState::default());
        let restored_reserve = restored.response_token_estimates.reserve_for(
            "model-a",
            "api",
            &RecoveryState::default(),
        );
        assert_eq!(
            original_reserve.output_tokens,
            restored_reserve.output_tokens
        );
    }

    #[test]
    fn session_state_roundtrip() {
        let mut stats = PipelineStats::default();
        let feedback = ContextFeedback::from_usage(0, 900, 100, 500, false);
        stats.record("claude-sonnet-4-6", "repl", &feedback);

        let mut latches = SessionLatches::default();
        latches.latch_cache_scope(CacheScope::Global, 1);
        latches.latch_header("anthropic-beta", "prompt-caching-2024-07-31", 1);

        let mut recovery = RecoveryState::default();
        recovery.record_ptl_error();
        recovery.record_output_escalation();

        let value = serialize_session_state(&stats, &latches, &recovery).unwrap();
        let restored = deserialize_session_state(&value).unwrap();

        assert_eq!(restored.stats.turns_executed, 1);
        assert!(restored.stats.avg_cache_hit_ratio > 0.8);
        assert_eq!(restored.latches.cache_scope, Some(CacheScope::Global));
        assert!(restored.latches.has_header("anthropic-beta"));
        // PTL errors cleared on restore, escalation preserved
        assert_eq!(restored.recovery.consecutive_ptl_errors, 0);
        assert_eq!(restored.recovery.max_output_escalation_count, 1);
    }

    #[test]
    fn deserialize_null_returns_none() {
        assert!(deserialize_session_state(&Value::Null).is_none());
    }

    #[test]
    fn deserialize_garbage_returns_none() {
        let garbage = serde_json::json!({"not": "a pipeline state"});
        assert!(deserialize_session_state(&garbage).is_none());
    }

    #[test]
    fn backward_compat_missing_field_uses_default() {
        // Simulate: old checkpoint saved with fewer fields in latches.
        // serde(default) on missing fields should produce a valid SessionLatches.
        let stats = PipelineStats::default();
        let latches = SessionLatches::default();
        let recovery = RecoveryState::default();
        let value = serialize_session_state(&stats, &latches, &recovery).unwrap();

        // Tamper: remove a field from latches to simulate schema evolution
        let mut obj = value.as_object().unwrap().clone();
        obj.insert("latches".into(), serde_json::json!({}));
        let tampered = Value::Object(obj);

        let restored = deserialize_session_state(&tampered).unwrap();
        assert_eq!(restored.latches.cache_scope, None);
        assert!(restored.latches.beta_headers.is_empty());
    }

    #[test]
    fn backward_compat_missing_pipeline_state_returns_none() {
        // Old checkpoints have no pipeline_state field — should gracefully return None
        assert!(deserialize_session_state(&Value::Null).is_none());
        assert!(deserialize_session_state(&serde_json::json!({})).is_none());
    }

    /// Review gap: the original `deserialize_session_state` collapsed
    /// "missing pipeline_state" and "corrupt pipeline_state" into the same
    /// `None`, leaving operators blind to restoration failures. The
    /// fallible twin distinguishes:
    ///   - `Ok(None)` → null / missing (old checkpoint, legitimate)
    ///   - `Ok(Some(_))` → parsed successfully
    ///   - `Err(_)` → present but corrupt / schema-broken; caller MUST log
    #[test]
    fn fallible_variant_distinguishes_missing_from_corrupt() {
        // Missing → Ok(None)
        let res = deserialize_session_state_fallible(&Value::Null);
        assert!(matches!(res, Ok(None)));
        let res = deserialize_session_state_fallible(&serde_json::json!({}));
        assert!(matches!(res, Ok(None)));

        // Corrupt (right shape, wrong types) → Err
        let corrupt = serde_json::json!({
            "version": "not_a_number",
            "stats": {},
            "latches": {},
            "recovery": {}
        });
        let res = deserialize_session_state_fallible(&corrupt);
        assert!(
            res.is_err(),
            "corrupt payload must surface as Err, not Ok(None): {res:?}"
        );

        // Valid → Ok(Some)
        let valid = serialize_session_state(
            &PipelineStats::default(),
            &SessionLatches::default(),
            &RecoveryState::default(),
        )
        .unwrap();
        let res = deserialize_session_state_fallible(&valid);
        assert!(matches!(res, Ok(Some(_))));
    }

    /// Backward-compat shim: the non-fallible `deserialize_session_state`
    /// retains the old `Option<_>` signature for callers that don't want
    /// three-way handling, but behaviour is STILL to collapse corrupt →
    /// None. Document that convention so nobody removes the shim without
    /// updating every caller.
    #[test]
    fn legacy_non_fallible_variant_still_collapses_corrupt_to_none() {
        let corrupt =
            serde_json::json!({"version": "bad", "stats": {}, "latches": {}, "recovery": {}});
        assert!(
            deserialize_session_state(&corrupt).is_none(),
            "legacy API collapses corrupt to None for back-compat — callers wanting \
             the distinction must use deserialize_session_state_fallible()"
        );
    }
}
