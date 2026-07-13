//! In-process, session-keyed reserve state for the bridge pipeline.
//!
//! The bridge assembles context with a per-request `PipelineSession`, so no
//! in-process state carries response-token reserves from one LLM round to the
//! next. Journal-tail replay (`load_bridge_pipeline_baseline`) cannot close
//! that gap within a turn: full LLM capture is a per-request debug opt-in
//! (`x-mo-full-llm-capture`), and `TurnEventBuffer` flushes at turn end, so
//! round N of a single-turn session finds nothing from rounds 1..N-1 in the
//! journal. The PR559 pilot measured the consequence: the planner's output
//! reserve stayed pinned at the 500-token cold floor in 568/568 decisions.
//!
//! This registry is the always-on carrier: usage is recorded the moment the
//! bridge parses a provider response, and the next assembly for the same
//! session seeds its planner stats from here. Journal replay remains the
//! cold-start fallback after a server restart.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use astra_turn_core::context_feedback::ContextFeedback;
use astra_turn_core::pipeline_stats::PipelineStats;

use crate::turn::token_usage::TokenUsage;

/// Sessions tracked before least-recently-used eviction kicks in. Entries are
/// a few KB each (a percentile digest per model bucket), so the cap bounds
/// memory without ever evicting a live experiment session in practice.
const MAX_SESSIONS: usize = 512;

#[derive(Default)]
pub(crate) struct SessionReserveRegistry {
    entries: Mutex<HashMap<String, ReserveEntry>>,
}

struct ReserveEntry {
    stats: PipelineStats,
    hit_ratio_sum: f64,
    hit_ratio_samples: u32,
    last_used: Instant,
}

impl SessionReserveRegistry {
    /// Record one observed LLM response for `session_id`.
    ///
    /// Advances the round counter (the planner's `turn_index`), folds the
    /// cache-hit ratio into a running mean, and — when the response produced
    /// output — records the completion into the response-token reserve digest
    /// under the same `(model, query_source)` bucket the bridge planner reads.
    pub(crate) fn record_response_usage(&self, session_id: &str, model: &str, usage: &TokenUsage) {
        if session_id.is_empty() || model.is_empty() || usage.is_empty() {
            return;
        }
        let total_input = usage
            .input_tokens
            .saturating_add(usage.cached_input_tokens)
            .saturating_add(usage.cache_creation_tokens);
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        let entry = entries
            .entry(session_id.to_string())
            .or_insert_with(|| ReserveEntry {
                stats: PipelineStats::default(),
                hit_ratio_sum: 0.0,
                hit_ratio_samples: 0,
                last_used: now,
            });
        entry.last_used = now;
        entry.stats.turns_executed = entry.stats.turns_executed.saturating_add(1);
        if total_input > 0 {
            entry.hit_ratio_sum += usage.cached_input_tokens as f64 / total_input as f64;
            entry.hit_ratio_samples = entry.hit_ratio_samples.saturating_add(1);
            entry.stats.avg_cache_hit_ratio =
                entry.hit_ratio_sum / f64::from(entry.hit_ratio_samples);
        }
        if usage.output_tokens > 0 {
            let feedback = ContextFeedback::from_usage(
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_tokens,
                usage.output_tokens,
                false,
            );
            entry.stats.response_token_estimates.record(
                model,
                crate::turn::prompt_cache::BRIDGE_PIPELINE_QUERY_SOURCE,
                &feedback,
            );
        }
        if entries.len() > MAX_SESSIONS {
            evict_least_recently_used(&mut entries);
        }
    }

    /// Clone the accumulated planner stats for `session_id`, refreshing its
    /// eviction clock. `None` until the session's first recorded response.
    pub(crate) fn stats_snapshot(&self, session_id: &str) -> Option<PipelineStats> {
        let mut entries = lock(&self.entries);
        let entry = entries.get_mut(session_id)?;
        entry.last_used = Instant::now();
        Some(entry.stats.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.entries).len()
    }
}

fn lock(
    entries: &Mutex<HashMap<String, ReserveEntry>>,
) -> MutexGuard<'_, HashMap<String, ReserveEntry>> {
    entries.lock().unwrap_or_else(PoisonError::into_inner)
}

fn evict_least_recently_used(entries: &mut HashMap<String, ReserveEntry>) {
    while entries.len() > MAX_SESSIONS {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

fn global() -> &'static SessionReserveRegistry {
    static REGISTRY: OnceLock<SessionReserveRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SessionReserveRegistry::default)
}

/// Record into the process-global registry. See
/// [`SessionReserveRegistry::record_response_usage`].
pub(crate) fn record_response_usage(session_id: &str, model: &str, usage: &TokenUsage) {
    global().record_response_usage(session_id, model, usage);
}

/// Snapshot from the process-global registry. See
/// [`SessionReserveRegistry::stats_snapshot`].
pub(crate) fn stats_snapshot(session_id: &str) -> Option<PipelineStats> {
    global().stats_snapshot(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            cache_creation_tokens: 0,
            output_tokens: output,
        }
    }

    #[test]
    fn record_then_snapshot_seeds_p75_reserve() {
        let registry = SessionReserveRegistry::default();
        for output in [4_000, 4_000, 4_000, 1_000] {
            registry.record_response_usage("sess-p75", "model-a", &usage(1_000, 9_000, output));
        }
        let stats = registry.stats_snapshot("sess-p75").expect("entry exists");
        assert_eq!(stats.turns_executed, 4);
        assert!((stats.avg_cache_hit_ratio - 0.9).abs() < 1e-9);
        let reserves = stats.response_token_estimates.reserve_for(
            "model-a",
            crate::turn::prompt_cache::BRIDGE_PIPELINE_QUERY_SOURCE,
            &astra_turn_core::recovery_state::RecoveryState::default(),
        );
        assert_eq!(
            reserves.output_tokens, 4_000,
            "p75 of recorded completions must reach the planner reserve"
        );
    }

    #[test]
    fn snapshot_unknown_session_is_none() {
        let registry = SessionReserveRegistry::default();
        assert!(registry.stats_snapshot("sess-never-seen").is_none());
    }

    #[test]
    fn zero_output_rounds_count_turns_but_not_reserves() {
        let registry = SessionReserveRegistry::default();
        registry.record_response_usage("sess-zero-out", "model-a", &usage(1_000, 0, 0));
        let stats = registry.stats_snapshot("sess-zero-out").expect("entry");
        assert_eq!(stats.turns_executed, 1);
        let reserves = stats.response_token_estimates.reserve_for(
            "model-a",
            crate::turn::prompt_cache::BRIDGE_PIPELINE_QUERY_SOURCE,
            &astra_turn_core::recovery_state::RecoveryState::default(),
        );
        assert_eq!(
            reserves.output_tokens, 500,
            "no completions recorded — reserve must stay on the cold floor"
        );
    }

    #[test]
    fn empty_session_or_model_or_usage_is_ignored() {
        let registry = SessionReserveRegistry::default();
        registry.record_response_usage("", "model-a", &usage(1, 1, 1));
        registry.record_response_usage("sess-guards", "", &usage(1, 1, 1));
        registry.record_response_usage("sess-guards", "model-a", &usage(0, 0, 0));
        assert_eq!(registry.len(), 0);
        assert!(registry.stats_snapshot("sess-guards").is_none());
    }

    #[test]
    fn eviction_drops_least_recently_used_sessions() {
        let registry = SessionReserveRegistry::default();
        for i in 0..(MAX_SESSIONS + 3) {
            registry.record_response_usage(
                &format!("sess-evict-{i}"),
                "model-a",
                &usage(100, 0, 100),
            );
        }
        assert_eq!(registry.len(), MAX_SESSIONS);
        assert!(
            registry.stats_snapshot("sess-evict-0").is_none(),
            "oldest entries must be evicted first"
        );
        assert!(
            registry
                .stats_snapshot(&format!("sess-evict-{}", MAX_SESSIONS + 2))
                .is_some(),
            "newest entry must survive eviction"
        );
    }
}
