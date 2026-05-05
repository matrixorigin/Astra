//! Serialization support for PipelineStats persistence.
//!
//! Enables warm-start: save stats at session end, reload on resume.
//! This is the Phase 12 foundation — persistence layer uses these
//! functions to store/load the cross-turn accumulated state.

use crate::pipeline_stats::PipelineStats;

/// Serialize PipelineStats to JSON bytes for persistence.
pub fn serialize_stats(stats: &PipelineStats) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(stats)
}

/// Deserialize PipelineStats from JSON bytes (loaded from persistence).
pub fn deserialize_stats(bytes: &[u8]) -> Result<PipelineStats, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_feedback::ContextFeedback;

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

        let original_reserve = stats
            .response_token_estimates
            .reserve_for("model-a", "api", &crate::recovery_state::RecoveryState::default());
        let restored_reserve = restored
            .response_token_estimates
            .reserve_for("model-a", "api", &crate::recovery_state::RecoveryState::default());
        assert_eq!(original_reserve.output_tokens, restored_reserve.output_tokens);
    }
}
