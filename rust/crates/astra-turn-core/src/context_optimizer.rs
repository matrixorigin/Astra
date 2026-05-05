//! Context pipeline Optimize phase — transform bound content into a
//! cache-aligned, budget-fitted, provider-specific arrangement.
//!
//! The optimizer operates within `OptimizeLimits`: each transformation
//! has an independent boolean gate. Closed gates produce trace entries
//! showing what *could have* happened but didn't.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction_types::CompactionTier;
use crate::context_binder::ContextBound;
use crate::context_planner::ContextPlan;
use crate::microcompact::{CompactStrategy, PromptCacheProtocol};
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_config::ProviderCachePolicy;
use crate::section_types::{BoundSection, CacheScope, SectionKind};
use crate::session_latches::SessionLatches;

/// A cache marker placed in the optimized output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMarker {
    pub after_section_index: usize,
    pub scope: CacheScope,
    pub cumulative_tokens: u32,
}

/// A tool result that was spilled to disk during optimization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpilledEntry {
    pub call_id: String,
    pub tool_name: String,
    pub original_tokens: u32,
    pub path: String,
}

/// Record of a skipped optimization step (for EXPLAIN).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedOptimization {
    pub step: String,
    pub reason: String,
}

/// Statistics from the optimize phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OptimizeStats {
    pub tool_results_cleared: u32,
    pub tokens_cleared: u32,
    pub schemas_pruned: u32,
    pub entries_spilled: u32,
    pub sections_reordered: u32,
    pub skipped: Vec<SkippedOptimization>,
}

/// Output of the Optimize phase.
#[derive(Debug)]
pub struct ContextOptimized {
    pub sections: Vec<BoundSection>,
    pub messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
    pub cache_markers: Vec<CacheMarker>,
    pub spilled: Vec<SpilledEntry>,
    pub stats: OptimizeStats,
}

/// Execute the Optimize phase.
///
/// Transforms bound content into a cache-aligned, budget-fitted arrangement.
/// All transformations are gated by `limits`. Closed gates produce trace
/// entries in `stats.skipped`.
pub fn optimize(
    plan: &ContextPlan,
    bound: ContextBound,
    latches: &SessionLatches,
    policy: &ProviderCachePolicy,
    limits: &OptimizeLimits,
    current_turn: u32,
) -> ContextOptimized {
    let ContextBound {
        mut sections,
        mut messages,
        mut tool_schemas,
    } = bound;

    let mut stats = OptimizeStats::default();

    // 1. ORDER: cache-align sections within reorderable groups
    if limits.allow_reorder {
        let moves = cache_align_sections(&mut sections, limits.max_reorder_moves);
        stats.sections_reordered = moves;
    } else {
        stats.skipped.push(SkippedOptimization {
            step: "reorder".into(),
            reason: "allow_reorder gate is closed".into(),
        });
    }

    // 2. COMPACT: apply tier-appropriate compaction to message history
    match plan.compact_tier {
        CompactionTier::Normal => {
            // No compaction needed
        }
        CompactionTier::TrimSchemas => {
            if limits.allow_schema_pruning {
                stats.schemas_pruned = prune_tool_schemas(&mut tool_schemas);
            } else {
                stats.skipped.push(SkippedOptimization {
                    step: "schema_pruning".into(),
                    reason: "allow_schema_pruning gate is closed".into(),
                });
            }
        }
        CompactionTier::CompactHistory | CompactionTier::AggressivePrune => {
            if limits.allow_schema_pruning {
                stats.schemas_pruned = prune_tool_schemas(&mut tool_schemas);
            }

            if limits.allow_tool_result_clearing {
                let cleared = compact_tool_results_gated(
                    &mut messages,
                    plan.compact_tier,
                    plan.pressure.value,
                    policy.compact_strategy,
                    limits.max_clear_tokens,
                );
                stats.tool_results_cleared = cleared.count;
                stats.tokens_cleared = cleared.tokens;
            } else {
                stats.skipped.push(SkippedOptimization {
                    step: "tool_result_clearing".into(),
                    reason: "allow_tool_result_clearing gate is closed".into(),
                });
            }

            if plan.compact_tier == CompactionTier::AggressivePrune {
                if limits.allow_round_dropping {
                    let dropped = drop_oldest_rounds(&mut messages, plan.pressure.value);
                    stats.tokens_cleared += dropped;
                } else {
                    stats.skipped.push(SkippedOptimization {
                        step: "round_dropping".into(),
                        reason: "allow_round_dropping gate is closed".into(),
                    });
                }
            }
        }
    }

    // 3. SPILL: persist oversized sections to disk
    let spilled = if limits.allow_spill {
        spill_oversized_sections(&mut sections, &mut stats)
    } else {
        stats.skipped.push(SkippedOptimization {
            step: "spill".into(),
            reason: "allow_spill gate is closed".into(),
        });
        Vec::new()
    };

    // 4. CACHE MARKERS: place based on provider protocol
    let cache_markers = place_cache_markers(&sections, policy, latches, current_turn);

    ContextOptimized {
        sections,
        messages,
        tool_schemas,
        cache_markers,
        spilled,
        stats,
    }
}

struct ClearResult {
    count: u32,
    tokens: u32,
}

/// Gated tool result clearing with circuit breaker.
/// Prune tool schemas: remove `description` fields to save tokens.
/// Returns the number of schemas pruned.
fn prune_tool_schemas(schemas: &mut [Value]) -> u32 {
    let mut pruned = 0u32;
    for schema in schemas.iter_mut() {
        if let Some(func) = schema.get_mut("function") {
            if func.get("description").is_some() {
                func.as_object_mut().unwrap().remove("description");
                pruned += 1;
            }
            // Also strip parameter descriptions
            if let Some(params) = func.get_mut("parameters") {
                if let Some(props) = params.get_mut("properties") {
                    if let Some(obj) = props.as_object_mut() {
                        for (_key, prop) in obj.iter_mut() {
                            if let Some(p) = prop.as_object_mut() {
                                p.remove("description");
                            }
                        }
                    }
                }
            }
        }
    }
    pruned
}

/// Drop the oldest assistant/user round pairs under extreme pressure.
/// Returns estimated tokens dropped.
fn drop_oldest_rounds(messages: &mut Vec<Value>, pressure: f64) -> u32 {
    if messages.len() < 4 {
        return 0; // Need at least 2 rounds to drop one
    }
    // Drop fraction scales with pressure: at 0.9 drop 1/6, at 1.0 drop 1/3
    let fraction = ((pressure - 0.85) * 2.0).clamp(0.0, 0.5);
    let total_rounds = messages.len() / 2;
    let rounds_to_drop = ((total_rounds as f64 * fraction) as usize).max(1);
    let messages_to_drop = (rounds_to_drop * 2).min(messages.len().saturating_sub(2));

    if messages_to_drop == 0 {
        return 0;
    }

    let tokens_dropped: u32 = messages[..messages_to_drop]
        .iter()
        .map(|m| {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            (content.len() as u32) / 4 // rough token estimate
        })
        .sum();

    messages.drain(..messages_to_drop);
    tokens_dropped
}

/// Spill oversized sections: sections above a threshold get replaced with
/// a `SpilledEntry` reference. The actual content is stored externally.
/// Threshold: 10000 tokens (~40KB of text).
fn spill_oversized_sections(
    sections: &mut [BoundSection],
    stats: &mut OptimizeStats,
) -> Vec<SpilledEntry> {
    const SPILL_THRESHOLD_TOKENS: u32 = 10_000;
    let mut spilled = Vec::new();

    for section in sections.iter_mut() {
        if section.actual_tokens > SPILL_THRESHOLD_TOKENS {
            // Don't spill Identity or Constraints (semantic anchors)
            if matches!(
                section.plan.kind,
                SectionKind::Identity | SectionKind::Constraints
            ) {
                continue;
            }
            let path = format!("spill/{:?}_{}.txt", section.plan.kind, spilled.len());
            spilled.push(SpilledEntry {
                call_id: String::new(),
                tool_name: format!("{:?}", section.plan.kind),
                original_tokens: section.actual_tokens,
                path: path.clone(),
            });
            // Replace artifact with a spill reference
            section.artifact = crate::section_types::SectionArtifact::SpillReference {
                path,
                original_tokens: section.actual_tokens,
            };
            section.actual_tokens = 20; // reference is ~20 tokens
            stats.entries_spilled += 1;
        }
    }

    spilled
}

/// `max_clear_tokens` caps total tokens cleared to prevent over-compaction.
fn compact_tool_results_gated(
    messages: &mut [Value],
    tier: CompactionTier,
    pressure: f64,
    strategy: CompactStrategy,
    max_clear_tokens: u32,
) -> ClearResult {
    // AggressivePrune escalates effective pressure to clear more aggressively
    let effective_pressure = match tier {
        CompactionTier::AggressivePrune => (pressure * 1.2).min(1.0),
        _ => pressure,
    };
    let stats =
        crate::microcompact::compact_tool_results_adaptive(messages, effective_pressure, strategy);
    let tokens = (stats.tokens_saved as u32).min(max_clear_tokens);
    ClearResult {
        count: stats.results_compacted as u32,
        tokens,
    }
}

/// Cache-align sections: sort within reorderable groups by cache scope.
/// Returns the total displaced positions required by the proposed reorder. If
/// that count exceeds `max_moves`, no reorder is applied and the returned value
/// still reports the skipped work for optimizer stats/explainability.
fn cache_align_sections(sections: &mut [BoundSection], max_moves: u32) -> u32 {
    // Only reorder within groups that have the same scope
    // Don't reorder Identity and Constraints (they're semantic anchors)
    let reorderable: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            !matches!(
                s.plan.kind,
                SectionKind::Identity | SectionKind::Constraints
            )
        })
        .map(|(i, _)| i)
        .collect();

    if reorderable.len() < 2 {
        return 0;
    }

    // Compute desired order by cache scope
    let mut sorted_indices = reorderable.clone();
    sorted_indices.sort_by_key(|&i| sections[i].plan.scope.order());

    // Count displacements
    let mut moves = 0u32;
    for (pos, &target_idx) in sorted_indices.iter().enumerate() {
        if reorderable[pos] != target_idx {
            moves += 1;
        }
    }

    // Apply sort in-place using swap instead of clone
    if moves <= max_moves && moves > 0 {
        // Build a permutation map and apply it via cycle-sort (zero allocations beyond the index vec)
        let mut perm: Vec<usize> = (0..reorderable.len()).collect();
        perm.sort_by_key(|&i| sections[reorderable[i]].plan.scope.order());

        let mut visited = vec![false; perm.len()];
        for start in 0..perm.len() {
            if visited[start] || perm[start] == start {
                visited[start] = true;
                continue;
            }
            let mut current = start;
            loop {
                visited[current] = true;
                let next = perm[current];
                if next == start {
                    break;
                }
                sections.swap(reorderable[current], reorderable[next]);
                current = next;
            }
            // Final swap to place `start` element
            sections.swap(reorderable[start], reorderable[perm[start]]);
        }
    }

    moves
}

/// Place cache markers at scope boundaries.
/// `latches` reserved for future latch-aware marker placement (e.g. skip
/// marker if latch flip would invalidate prefix).
fn place_cache_markers(
    sections: &[BoundSection],
    policy: &ProviderCachePolicy,
    latches: &SessionLatches,
    current_turn: u32,
) -> Vec<CacheMarker> {
    if policy.protocol == PromptCacheProtocol::Prefix {
        // Prefix caching: no explicit markers (provider does it automatically)
        return Vec::new();
    }

    // If any latch flipped this turn, suppress trailing markers to avoid
    // caching content that will change next turn.
    let latch_flipped = latches.any_flipped_this_turn(current_turn);

    let mut markers = Vec::new();
    let mut cumulative_tokens = 0u32;
    let mut last_scope = None;

    for (i, section) in sections.iter().enumerate() {
        cumulative_tokens += section.actual_tokens;
        let scope = section.plan.scope;

        if let Some(prev_scope) = last_scope {
            if prev_scope != scope && markers.len() < policy.max_markers as usize {
                // Skip None-scope markers if a latch flipped (content unstable)
                if latch_flipped && scope == CacheScope::None {
                    // Don't place marker before volatile content
                } else {
                    markers.push(CacheMarker {
                        after_section_index: i.saturating_sub(1),
                        scope: prev_scope,
                        cumulative_tokens: cumulative_tokens - section.actual_tokens,
                    });
                }
            }
        }
        last_scope = Some(scope);
    }

    markers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_binder::bind_all;
    use crate::context_planner::{PlanInput, plan_turn};
    use crate::context_sources::*;
    use crate::emergent_context::EmergentContext;
    use crate::microcompact::ProviderCacheStrategy;
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::pipeline_stats::PipelineStats;
    use crate::recovery_state::RecoveryState;
    use crate::session_latches::SessionLatches;
    use crate::token_accounting::TokenAccounting;
    use std::collections::HashMap;

    fn build_test_plan_and_bound() -> (ContextPlan, ContextBound, SessionLatches) {
        let statics = StaticSections::test_default();
        let agent = AgentContext::default();
        let latches = SessionLatches::default();
        let session = SessionContext {
            session_id: "s1".into(),
            run_id: "r1".into(),
            model_id: "m1".into(),
            model_limit: 100_000,
            provider_policy: ProviderCachePolicy::default(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "test project".into(),
            edge_profile: EdgeProfile::default(),
            self_model: None,
        };
        let turn = TurnState {
            messages: vec![],
            tool_results: vec![],
            tokens: TokenAccounting::default(),
            active_skills: vec![],
            recent_file_reads: HashMap::new(),
            remaining_turns: 10,
            turn_index: 1,
            recovery: RecoveryState::default(),
            last_user_message: "test".into(),
        };
        let external = ExternalSources {
            memory_snippets: Vec::new(),
            spill_dir: None,
        };
        let emergent = EmergentContext::default();
        let stats = PipelineStats::default();

        let sources = ContextSources {
            statics: &statics,
            agent: &agent,
            latches: &latches,
            session: &session,
            turn: &turn,
            external: &external,
            emergent: &emergent,
            stats: &stats,
        };

        let plan_input = PlanInput {
            tokens: &turn.tokens,
            model_limit: 100_000,
            recovery: &turn.recovery,
            latches: &latches,
            stats: &stats,
            provider_policy: &session.provider_policy,
            has_memory: false,
            model_id: "m1",
            query_source: "repl",
        };

        let plan = plan_turn(&plan_input);
        let bound = bind_all(&plan, &sources);
        (plan, bound, latches)
    }

    #[test]
    fn all_gates_closed_returns_unmodified() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let original_section_count = bound.sections.len();
        let original_message_count = bound.messages.len();
        let limits = OptimizeLimits::all_closed();
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);

        assert_eq!(result.sections.len(), original_section_count);
        assert_eq!(result.messages.len(), original_message_count);
        assert!(
            !result.stats.skipped.is_empty(),
            "closed gates should produce skipped entries"
        );
    }

    #[test]
    fn reorder_gate_closed_preserves_order() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let original_order: Vec<SectionKind> = bound.sections.iter().map(|s| s.plan.kind).collect();
        let limits = OptimizeLimits::all_closed();
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        let result_order: Vec<SectionKind> = result.sections.iter().map(|s| s.plan.kind).collect();

        assert_eq!(
            original_order, result_order,
            "order should be preserved when reorder gate is closed"
        );
    }

    fn test_bound_section(kind: SectionKind, scope: CacheScope, text: &str) -> BoundSection {
        BoundSection {
            plan: crate::section_types::PlannedSection {
                kind,
                scope,
                estimated_tokens: text.len() as u32,
                priority: crate::section_types::CompressionPriority::Normal,
                source: crate::section_types::SectionSource::Static,
            },
            artifact: crate::section_types::SectionArtifact::from_text(kind, text.to_string()),
            actual_tokens: text.len() as u32,
            bind_latency: std::time::Duration::ZERO,
        }
    }

    #[test]
    fn reorder_respects_max_moves_budget() {
        let mut sections = vec![
            test_bound_section(SectionKind::Skills, CacheScope::None, "none"),
            test_bound_section(SectionKind::RuntimeIdentity, CacheScope::Session, "session"),
            test_bound_section(SectionKind::ProjectContext, CacheScope::Global, "global"),
        ];
        let original_order: Vec<CacheScope> = sections.iter().map(|s| s.plan.scope).collect();

        let moves = cache_align_sections(&mut sections, 1);

        assert_eq!(moves, 2, "should report actual displaced positions");
        assert_eq!(
            sections.iter().map(|s| s.plan.scope).collect::<Vec<_>>(),
            original_order,
            "order must be preserved when required moves exceed max_moves"
        );
    }

    #[test]
    fn tool_result_clearing_gate_controls_microcompact() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::all_closed();
        // Gate is closed — no clearing should happen
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert_eq!(result.stats.tool_results_cleared, 0);
        assert!(
            result
                .stats
                .skipped
                .iter()
                .any(|s| s.step == "tool_result_clearing"
                    || s.step == "reorder"
                    || s.step == "spill")
        );
    }

    #[test]
    fn normal_tier_no_compaction() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        assert_eq!(plan.compact_tier, CompactionTier::Normal);
        let limits = OptimizeLimits::default();
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert_eq!(result.stats.tool_results_cleared, 0);
    }

    #[test]
    fn anthropic_places_cache_markers() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::default();
        let policy = ProviderCachePolicy::anthropic();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        // Anthropic should have markers at scope boundaries
        // (Global→Session or Session→None transitions)
        // The exact count depends on section layout
        // Just verify the mechanism runs
        assert!(result.cache_markers.len() <= policy.max_markers as usize);
    }

    #[test]
    fn prefix_provider_no_markers() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::default();
        let policy = ProviderCachePolicy::openai_compatible();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert!(
            result.cache_markers.is_empty(),
            "prefix caching should not produce explicit markers"
        );
    }

    #[test]
    fn produces_trace_with_skipped_gates() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::all_closed();
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        let skipped_steps: Vec<&str> = result
            .stats
            .skipped
            .iter()
            .map(|s| s.step.as_str())
            .collect();
        assert!(
            skipped_steps.contains(&"reorder"),
            "should record skipped reorder"
        );
        assert!(
            skipped_steps.contains(&"spill"),
            "should record skipped spill"
        );
    }

    #[test]
    fn schema_pruning_removes_low_priority_schemas() {
        let (mut plan, bound, latches) = build_test_plan_and_bound();
        plan.compact_tier = CompactionTier::TrimSchemas;
        let limits = OptimizeLimits {
            allow_schema_pruning: true,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        // Inject some tool schemas with descriptions to be pruned
        let mut bound = bound;
        bound.tool_schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "read_file", "description": "Read a file from disk", "parameters": {"type": "object", "properties": {"path": {"type": "string", "description": "File path"}}}}}),
            serde_json::json!({"type": "function", "function": {"name": "bash", "description": "Execute a shell command", "parameters": {"type": "object", "properties": {"command": {"type": "string", "description": "Command to run"}}}}}),
            serde_json::json!({"type": "function", "function": {"name": "grep", "description": "Search file contents", "parameters": {"type": "object", "properties": {"pattern": {"type": "string", "description": "Regex pattern"}, "path": {"type": "string", "description": "Search path"}}}}}),
            serde_json::json!({"type": "function", "function": {"name": "list_dir", "description": "List directory", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "depth": {"type": "integer"}}}}}),
        ];

        let original_count = bound.tool_schemas.len();
        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);

        // Under TrimSchemas tier, schemas should be pruned (descriptions removed or schemas dropped)
        assert!(
            result.stats.schemas_pruned > 0,
            "TrimSchemas tier should prune schemas"
        );
        // Should not remove ALL schemas
        assert!(
            !result.tool_schemas.is_empty(),
            "should keep at least some schemas"
        );
        assert!(
            result.tool_schemas.len() <= original_count,
            "should not add schemas"
        );
    }

    #[test]
    fn schema_pruning_gate_closed_does_nothing() {
        let (mut plan, bound, latches) = build_test_plan_and_bound();
        plan.compact_tier = CompactionTier::TrimSchemas;
        let mut limits = OptimizeLimits::all_closed();
        limits.allow_schema_pruning = false;
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        bound.tool_schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
        ];
        let original_count = bound.tool_schemas.len();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert_eq!(result.tool_schemas.len(), original_count);
        assert_eq!(result.stats.schemas_pruned, 0);
    }

    #[test]
    fn round_dropping_removes_oldest_rounds() {
        let (mut plan, bound, latches) = build_test_plan_and_bound();
        plan.compact_tier = CompactionTier::AggressivePrune;
        plan.pressure.value = 0.95;
        let limits = OptimizeLimits {
            allow_round_dropping: true,
            allow_tool_result_clearing: false, // isolate round dropping
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        // Create 6 rounds of assistant+user messages
        bound.messages = (0..12)
            .map(|i| {
                if i % 2 == 0 {
                    serde_json::json!({"role": "assistant", "content": format!("response {}", i/2)})
                } else {
                    serde_json::json!({"role": "user", "content": format!("query {}", i/2)})
                }
            })
            .collect();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);

        // Should have dropped at least 1 round (2 messages)
        assert!(
            result.messages.len() < 12,
            "round_dropping should remove messages, got {}",
            result.messages.len()
        );
        // Should keep the most recent messages
        let last = result.messages.last().unwrap();
        assert_eq!(last["content"], "query 5", "most recent should be kept");
    }

    #[test]
    fn round_dropping_gate_closed_preserves_all() {
        let (mut plan, bound, latches) = build_test_plan_and_bound();
        plan.compact_tier = CompactionTier::AggressivePrune;
        plan.pressure.value = 0.95;
        let limits = OptimizeLimits {
            allow_round_dropping: false,
            allow_tool_result_clearing: false,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        bound.messages = (0..12)
            .map(|i| {
                if i % 2 == 0 {
                    serde_json::json!({"role": "assistant", "content": format!("response {}", i/2)})
                } else {
                    serde_json::json!({"role": "user", "content": format!("query {}", i/2)})
                }
            })
            .collect();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert_eq!(result.messages.len(), 12);
    }

    #[test]
    fn spill_moves_large_sections_to_disk_entries() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits {
            allow_spill: true,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        // Create a bound with a very large section
        let mut bound = bound;
        let large_text = "x".repeat(50_000); // ~12500 tokens
        bound.sections.push(test_bound_section(
            SectionKind::ProjectContext,
            CacheScope::Session,
            &large_text,
        ));

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);

        // Spill should detect oversized sections and produce SpilledEntry records
        assert!(
            result.stats.entries_spilled > 0 || !result.spilled.is_empty(),
            "large sections should trigger spill"
        );
    }

    #[test]
    fn system_prompt_semantic_order_stable() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::default(); // reorder=false by default
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        // Identity should be first, Constraints should be second (or early)
        let kinds: Vec<SectionKind> = result.sections.iter().map(|s| s.plan.kind).collect();
        let identity_pos = kinds.iter().position(|k| *k == SectionKind::Identity);
        let constraints_pos = kinds.iter().position(|k| *k == SectionKind::Constraints);
        assert!(identity_pos.is_some(), "Identity must be present");
        assert!(constraints_pos.is_some(), "Constraints must be present");
        assert!(
            identity_pos.unwrap() < constraints_pos.unwrap(),
            "Identity should come before Constraints"
        );
    }
}
