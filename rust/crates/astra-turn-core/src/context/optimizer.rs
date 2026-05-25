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
use crate::section_types::{BoundSection, CacheScope, SectionArtifact, SectionKind};
use crate::session_latches::SessionLatches;
use crate::spill_backend::SpillBackend;

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
    optimize_with_spill(plan, bound, latches, policy, limits, current_turn, None)
}

/// Same as [`optimize`] but accepts an optional spill backend. When the gate
/// `allow_spill` is open and a backend is supplied, oversized non-anchor
/// sections are persisted via the backend and replaced with
/// `SectionArtifact::SpillReference`. Without a backend the optimizer keeps
/// the conservative behaviour (preserve content + emit skipped-optimization
/// trace entry).
pub fn optimize_with_spill(
    plan: &ContextPlan,
    bound: ContextBound,
    latches: &SessionLatches,
    policy: &ProviderCachePolicy,
    limits: &OptimizeLimits,
    current_turn: u32,
    spill_backend: Option<&dyn SpillBackend>,
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
                stats.schemas_pruned = prune_tool_schemas(&mut tool_schemas, plan.compact_tier);
            } else {
                stats.skipped.push(SkippedOptimization {
                    step: "schema_pruning".into(),
                    reason: "allow_schema_pruning gate is closed".into(),
                });
            }
        }
        CompactionTier::CompactHistory | CompactionTier::AggressivePrune => {
            if limits.allow_schema_pruning {
                stats.schemas_pruned = prune_tool_schemas(&mut tool_schemas, plan.compact_tier);
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
                if cleared.skipped_over_budget {
                    stats.skipped.push(SkippedOptimization {
                        step: "tool_result_clearing".into(),
                        reason: format!(
                            "clearing would exceed max_clear_tokens={}",
                            limits.max_clear_tokens
                        ),
                    });
                }
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

    // 3. SPILL: persist oversized sections to the spill backend
    let spilled = if limits.allow_spill {
        spill_oversized_sections(&mut sections, &mut stats, spill_backend, plan, current_turn)
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
    skipped_over_budget: bool,
}

/// Rehydrate previously-spilled sections in `sections` by replacing any
/// `SectionArtifact::SpillReference` with the loaded original text
/// (as `SystemText` / `RuntimeText` / `MemoryText` / `HistorySummary`
/// depending on the section's kind).
///
/// This is the Phase-12 consumer side of spill: the optimizer *creates*
/// `SpillReference` during `spill_oversized_sections`; this function
/// *resolves* them back for downstream serialization or session-resume.
///
/// Behaviour:
/// - Missing scheme / load error → fail-open with a placeholder string
///   (`SectionArtifact::rehydrate` handles that) AND record a
///   `SkippedOptimization` trace entry so explain UI can surface it.
/// - `actual_tokens` is recomputed from the resolved text length so
///   downstream budget accounting stays honest.
/// - Inline artifacts are untouched (fast path).
pub fn rehydrate_sections(
    sections: &mut [BoundSection],
    registry: &crate::spill_backend::SpillRegistry,
) -> OptimizeStats {
    let mut stats = OptimizeStats::default();
    for section in sections.iter_mut() {
        let Some((path, _)) = section.artifact.spill_locator() else {
            continue;
        };
        let path = path.to_string();
        // Route through registry + SectionArtifact::rehydrate so fail-open
        // logic is in one place.
        let resolved = section.artifact.rehydrate(registry).into_owned();
        let is_placeholder = resolved.starts_with("[spilled content unavailable");
        if is_placeholder {
            stats.skipped.push(SkippedOptimization {
                step: "rehydrate".into(),
                reason: format!("failed to load spilled section from {path}"),
            });
            // Keep the spill reference intact — downstream consumers may
            // choose to retry, and overwriting with a placeholder would
            // permanently poison the section.
            continue;
        }
        section.actual_tokens = crate::section_types::estimate_text_tokens(&resolved);
        section.artifact = SectionArtifact::from_text(section.plan.kind, resolved);
    }
    stats
}

/// Prune tool schemas in-place using the shared 4-tier pruning strategy.
///
/// Delegates to [`crate::tool::schema::prune::prune_tool_schemas`] so the
/// pipeline and runtime share exactly one pruning implementation.
/// Returns the number of schemas whose `Value` representation changed —
/// observational only (for stats traces); the canonical proof is the
/// mutated `schemas` vec itself. Uses `Value` equality (no re-serialization)
/// to keep the hot path allocation-light.
fn prune_tool_schemas(schemas: &mut Vec<Value>, tier: CompactionTier) -> u32 {
    let pruned = crate::tool::schema::prune::prune_tool_schemas(schemas, tier);
    let touched = pruned
        .iter()
        .zip(schemas.iter())
        .filter(|(after, before)| after != before)
        .count();
    *schemas = pruned;
    u32::try_from(touched).unwrap_or(u32::MAX)
}

/// Drop the oldest assistant/user round pairs under extreme pressure.
/// Returns estimated tokens dropped.
fn drop_oldest_rounds(messages: &mut Vec<Value>, pressure: f64) -> u32 {
    let droppable_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) != Some("system"))
        .map(|(idx, _)| idx)
        .collect();

    if droppable_indices.len() < 4 {
        return 0; // Need at least 2 rounds to drop one
    }
    // Drop fraction scales with pressure: at 0.9 drop 1/6, at 1.0 drop 1/3
    let fraction = ((pressure - 0.85) * 2.0).clamp(0.0, 0.5);
    let total_rounds = droppable_indices.len() / 2;
    let rounds_to_drop = ((total_rounds as f64 * fraction) as usize).max(1);
    let messages_to_drop = (rounds_to_drop * 2).min(droppable_indices.len().saturating_sub(2));

    if messages_to_drop == 0 {
        return 0;
    }

    let tokens_dropped: u32 = droppable_indices
        .iter()
        .take(messages_to_drop)
        .map(|&idx| &messages[idx])
        .map(|m| {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            (content.len() as u32) / 4 // rough token estimate
        })
        .sum();

    for &idx in droppable_indices.iter().take(messages_to_drop).rev() {
        messages.remove(idx);
    }
    tokens_dropped
}

/// Spill oversized sections: sections above `SPILL_THRESHOLD_TOKENS` are
/// persisted via `backend` (if provided) and their in-prompt artifact is
/// replaced with a `SpillReference`. `Identity` and `Constraints` sections
/// are anchors and always preserved. Without a backend the function
/// preserves content and records a skipped-optimization trace so operators
/// can see that spill was attempted but had no sink.
fn spill_oversized_sections(
    sections: &mut [BoundSection],
    stats: &mut OptimizeStats,
    backend: Option<&dyn SpillBackend>,
    plan: &ContextPlan,
    current_turn: u32,
) -> Vec<SpilledEntry> {
    const SPILL_THRESHOLD_TOKENS: u32 = 10_000;
    let mut spilled = Vec::new();
    let mut saw_candidate_without_backend = false;

    for (idx, section) in sections.iter_mut().enumerate() {
        if section.actual_tokens <= SPILL_THRESHOLD_TOKENS {
            continue;
        }
        // Semantic anchors and goal continuity must remain inline. A spill
        // reference saves tokens but hides the very state needed to resume
        // correctly after compaction.
        if matches!(
            section.plan.kind,
            SectionKind::Identity | SectionKind::Constraints | SectionKind::WorkingMemory
        ) {
            if section.plan.kind == SectionKind::WorkingMemory {
                stats.skipped.push(SkippedOptimization {
                    step: "spill".into(),
                    reason: "working memory carries goal continuity and must remain inline".into(),
                });
            }
            continue;
        }

        let Some(backend) = backend else {
            saw_candidate_without_backend = true;
            continue;
        };

        // Only text-bearing artifacts can be spilled.
        let Some(text) = section.artifact.text() else {
            continue;
        };
        let original_tokens = section.actual_tokens;
        let key_hint = format!(
            "sec{idx}-turn{current_turn}-tier{tier:?}-{kind:?}",
            kind = section.plan.kind,
            tier = plan.compact_tier,
        );

        match backend.store(&key_hint, text.as_bytes()) {
            Ok(path) => {
                spilled.push(SpilledEntry {
                    call_id: format!("section-{idx}"),
                    tool_name: format!("{:?}", section.plan.kind),
                    original_tokens,
                    path: path.clone(),
                });
                section.artifact = SectionArtifact::SpillReference {
                    path,
                    original_tokens,
                };
                section.actual_tokens = 0;
                stats.entries_spilled = stats.entries_spilled.saturating_add(1);
                stats.tokens_cleared = stats.tokens_cleared.saturating_add(original_tokens);
            }
            Err(err) => {
                stats.skipped.push(SkippedOptimization {
                    step: "spill".into(),
                    reason: format!("spill backend error for {:?}: {err}", section.plan.kind),
                });
            }
        }
    }

    if saw_candidate_without_backend {
        stats.skipped.push(SkippedOptimization {
            step: "spill".into(),
            reason: "oversized sections present but no spill backend configured; content preserved"
                .into(),
        });
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
    if max_clear_tokens == 0 {
        return ClearResult {
            count: 0,
            tokens: 0,
            skipped_over_budget: true,
        };
    }

    // AggressivePrune escalates effective pressure to clear more aggressively
    let effective_pressure = match tier {
        CompactionTier::AggressivePrune => (pressure * 1.2).min(1.0),
        _ => pressure,
    };

    let mut candidate = messages.to_vec();
    let stats = crate::microcompact::compact_tool_results_adaptive(
        &mut candidate,
        effective_pressure,
        strategy,
    );
    let tokens = u32::try_from(stats.tokens_saved).unwrap_or(u32::MAX);
    if tokens > max_clear_tokens {
        return ClearResult {
            count: 0,
            tokens: 0,
            skipped_over_budget: true,
        };
    }
    messages.clone_from_slice(&candidate);
    ClearResult {
        count: u32::try_from(stats.results_compacted).unwrap_or(u32::MAX),
        tokens,
        skipped_over_budget: false,
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

    if moves <= max_moves && moves > 0 {
        let sorted_sections: Vec<BoundSection> = sorted_indices
            .iter()
            .map(|&idx| sections[idx].clone())
            .collect();
        for (target_idx, sorted_section) in reorderable.iter().zip(sorted_sections) {
            sections[*target_idx] = sorted_section;
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
    use crate::context_planner::{plan_turn, PlanInput};
    use crate::context_sources::*;
    use crate::emergent_context::EmergentContext;
    use crate::microcompact::{CompactStrategy, ProviderCacheStrategy};
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::pipeline_stats::PipelineStats;
    use crate::recovery_state::RecoveryState;
    use crate::section_types::{CompressionPriority, PlannedSection, SectionSource};
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
            provider_name: "anthropic".into(),
            model_limit: 100_000,
            provider_policy: ProviderCachePolicy::default(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "test project".into(),
            edge_profile: EdgeProfile::default(),
            self_model: None,
            deferred_tools_block: String::new(),
            skill_listing_block: String::new(),
            current_date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            user_id: None,
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
            memory_entries: Vec::new(),
            spill_dir: None,
            ..Default::default()
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
            working_memory: None,
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
    fn reorder_applies_length_three_cycle_correctly() {
        let mut sections = vec![
            test_bound_section(SectionKind::Skills, CacheScope::Session, "session"),
            test_bound_section(SectionKind::RuntimeIdentity, CacheScope::None, "none"),
            test_bound_section(SectionKind::ProjectContext, CacheScope::Global, "global"),
        ];

        let moves = cache_align_sections(&mut sections, 3);

        assert_eq!(moves, 3);
        assert_eq!(
            sections.iter().map(|s| s.plan.scope).collect::<Vec<_>>(),
            vec![CacheScope::Global, CacheScope::Session, CacheScope::None],
            "length-3 reorder cycles must produce fully sorted cache scopes"
        );
        assert_eq!(sections[0].text(), Some("global"));
        assert_eq!(sections[1].text(), Some("session"));
        assert_eq!(sections[2].text(), Some("none"));
    }

    #[test]
    fn reorder_never_moves_identity_or_constraints_anchors() {
        let mut sections = vec![
            test_bound_section(SectionKind::Skills, CacheScope::None, "none-a"),
            test_bound_section(SectionKind::Identity, CacheScope::Global, "identity"),
            test_bound_section(SectionKind::RuntimeIdentity, CacheScope::Session, "session"),
            test_bound_section(SectionKind::Constraints, CacheScope::Global, "constraints"),
            test_bound_section(SectionKind::ProjectContext, CacheScope::Global, "global"),
        ];

        let moves = cache_align_sections(&mut sections, 5);

        assert!(moves > 0);
        assert_eq!(sections[1].plan.kind, SectionKind::Identity);
        assert_eq!(sections[1].text(), Some("identity"));
        assert_eq!(sections[3].plan.kind, SectionKind::Constraints);
        assert_eq!(sections[3].text(), Some("constraints"));
        assert_eq!(sections[0].text(), Some("global"));
        assert_eq!(sections[2].text(), Some("session"));
        assert_eq!(sections[4].text(), Some("none-a"));
    }

    fn tool_result_messages(count: usize, content_len: usize) -> Vec<Value> {
        let mut messages = vec![serde_json::json!({
            "role": "assistant",
            "tool_calls": (0..count)
                .map(|i| serde_json::json!({
                    "id": format!("call_{i}"),
                    "function": {"name": "read_file", "arguments": "{}"}
                }))
                .collect::<Vec<_>>()
        })];
        for i in 0..count {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": "x".repeat(content_len),
            }));
        }
        messages
    }

    #[test]
    fn tool_result_clearing_gate_controls_microcompact() {
        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits::all_closed();
        // Gate is closed — no clearing should happen
        let policy = ProviderCachePolicy::default();

        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);
        assert_eq!(result.stats.tool_results_cleared, 0);
        assert!(result
            .stats
            .skipped
            .iter()
            .any(|s| s.step == "tool_result_clearing" || s.step == "reorder" || s.step == "spill"));
    }

    #[test]
    fn tool_result_clearing_skips_when_over_max_clear_tokens() {
        let mut messages = tool_result_messages(8, 12_000);

        let result = compact_tool_results_gated(
            &mut messages,
            CompactionTier::AggressivePrune,
            1.0,
            CompactStrategy::Normalized,
            1,
        );

        assert!(result.skipped_over_budget);
        assert_eq!(result.count, 0);
        assert_eq!(result.tokens, 0);
        assert!(
            messages.iter().all(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| !crate::microcompact::is_cleared_content(content))
                    .unwrap_or(true)
            }),
            "messages must remain unmodified when clearing exceeds the circuit breaker"
        );
    }

    #[test]
    fn tool_result_clearing_allows_exact_max_clear_tokens_boundary() {
        let mut probe = tool_result_messages(8, 12_000);
        let expected = compact_tool_results_gated(
            &mut probe,
            CompactionTier::AggressivePrune,
            1.0,
            CompactStrategy::Normalized,
            u32::MAX,
        )
        .tokens;
        assert!(
            expected > 0,
            "probe should identify compactable tool results"
        );

        let mut exact = tool_result_messages(8, 12_000);
        let exact_result = compact_tool_results_gated(
            &mut exact,
            CompactionTier::AggressivePrune,
            1.0,
            CompactStrategy::Normalized,
            expected,
        );
        assert!(!exact_result.skipped_over_budget);
        assert_eq!(exact_result.tokens, expected);

        let mut below = tool_result_messages(8, 12_000);
        let below_result = compact_tool_results_gated(
            &mut below,
            CompactionTier::AggressivePrune,
            1.0,
            CompactStrategy::Normalized,
            expected.saturating_sub(1),
        );
        assert!(below_result.skipped_over_budget);
        assert_eq!(below_result.tokens, 0);
        assert!(
            below.iter().all(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| !crate::microcompact::is_cleared_content(content))
                    .unwrap_or(true)
            }),
            "below-boundary rejection must leave messages unmodified"
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

        // Inject tool schemas with multi-sentence descriptions so TrimSchemas
        // (which truncates to the first sentence) actually mutates them.
        let mut bound = bound;
        bound.tool_schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "read_file", "description": "Read a file from disk. Supports optional line ranges and binary-safe decoding.", "parameters": {"type": "object", "properties": {"path": {"type": "string", "description": "File path. Accepts absolute or workspace-relative paths."}}}}}),
            serde_json::json!({"type": "function", "function": {"name": "bash", "description": "Execute a shell command. Runs inside the sandbox with a 2-minute default timeout.", "parameters": {"type": "object", "properties": {"command": {"type": "string", "description": "Command to run. Must be idempotent when possible to allow retries."}}}}}),
        ];

        let original_count = bound.tool_schemas.len();
        let result = optimize(&plan, bound, &latches, &policy, &limits, 1);

        // Under TrimSchemas tier, descriptions should be truncated to the first sentence.
        assert!(
            result.stats.schemas_pruned > 0,
            "TrimSchemas tier should touch schemas with multi-sentence descriptions"
        );
        let first_desc = result.tool_schemas[0]["function"]["description"]
            .as_str()
            .expect("description should still be present after TrimSchemas");
        assert!(
            !first_desc.contains("Supports optional line ranges"),
            "trailing sentences should be removed under TrimSchemas"
        );
        assert!(
            result.tool_schemas.len() == original_count,
            "TrimSchemas keeps schema count stable"
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
    fn schema_pruning_preserves_malformed_function_schema() {
        let mut schemas = vec![serde_json::json!({
            "type": "function",
            "function": "not-an-object"
        })];

        let pruned = prune_tool_schemas(&mut schemas, CompactionTier::TrimSchemas);

        assert_eq!(pruned, 0);
        assert_eq!(schemas[0]["function"], "not-an-object");
    }

    #[test]
    fn schema_pruning_preserves_malformed_nested_properties() {
        let original = vec![
            serde_json::json!({"type": "function", "function": {"name": "null_props", "parameters": {"type": "object", "properties": null}}}),
            serde_json::json!({"type": "function", "function": {"name": "array_props", "parameters": {"type": "object", "properties": []}}}),
            serde_json::json!({"type": "function", "function": {"name": "number_props", "parameters": {"type": "object", "properties": 42}}}),
        ];
        let mut schemas = original.clone();

        let pruned = prune_tool_schemas(&mut schemas, CompactionTier::TrimSchemas);

        assert_eq!(pruned, 0);
        assert_eq!(schemas, original);
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
    fn round_dropping_never_removes_system_messages() {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": "stable system"
        })];
        messages.extend((0..12).map(|i| {
            if i % 2 == 0 {
                serde_json::json!({"role": "assistant", "content": format!("response {}", i/2)})
            } else {
                serde_json::json!({"role": "user", "content": format!("query {}", i/2)})
            }
        }));

        let dropped = drop_oldest_rounds(&mut messages, 1.0);

        assert!(dropped > 0);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "stable system");
    }

    #[test]
    fn spill_preserves_large_sections_without_persistence() {
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

        assert_eq!(result.stats.entries_spilled, 0);
        assert!(result.spilled.is_empty());
        let preserved = result
            .sections
            .iter()
            .find(|section| section.text() == Some(large_text.as_str()))
            .expect("oversized text must survive when no persistence boundary exists");
        assert_eq!(preserved.actual_tokens, large_text.len() as u32);
        assert!(
            result
                .stats
                .skipped
                .iter()
                .any(|skipped| skipped.step == "spill"),
            "preserving an oversized section should be explicit in optimizer trace"
        );
    }

    #[test]
    fn spill_offloads_oversized_section_when_backend_configured() {
        use crate::spill_backend::FileSystemSpillBackend;
        use tempfile::TempDir;

        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits {
            allow_spill: true,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        let large_text = "Y".repeat(50_000);
        bound.sections.push(test_bound_section(
            SectionKind::ProjectContext,
            CacheScope::Session,
            &large_text,
        ));

        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let result =
            optimize_with_spill(&plan, bound, &latches, &policy, &limits, 1, Some(&backend));

        assert_eq!(result.stats.entries_spilled, 1);
        assert_eq!(result.spilled.len(), 1);
        let entry = &result.spilled[0];
        assert_eq!(entry.original_tokens, large_text.len() as u32);

        // The offloaded section must no longer carry the text inline.
        let offloaded = result
            .sections
            .iter()
            .find(|s| {
                matches!(
                    s.artifact,
                    crate::section_types::SectionArtifact::SpillReference { .. }
                )
            })
            .expect("oversized section should be replaced with a SpillReference");
        assert_eq!(offloaded.actual_tokens, 0);
        assert_eq!(offloaded.text(), None);

        // Tokens cleared accounting reflects the offload.
        assert!(result.stats.tokens_cleared >= large_text.len() as u32);

        // Persisted file contents match original text.
        let persisted = std::fs::read_to_string(&entry.path).unwrap();
        assert_eq!(persisted.len(), large_text.len());
        assert!(persisted.starts_with("YYYYYY"));
    }

    #[test]
    fn spill_never_offloads_identity_or_constraints_even_with_backend() {
        use crate::spill_backend::FileSystemSpillBackend;
        use tempfile::TempDir;

        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits {
            allow_spill: true,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        let large_text = "Z".repeat(50_000);
        bound.sections.push(test_bound_section(
            SectionKind::Identity,
            CacheScope::Global,
            &large_text,
        ));

        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let result =
            optimize_with_spill(&plan, bound, &latches, &policy, &limits, 1, Some(&backend));

        assert_eq!(result.stats.entries_spilled, 0);
        assert!(result.spilled.is_empty());
        let preserved = result
            .sections
            .iter()
            .find(|s| s.plan.kind == SectionKind::Identity && s.text() == Some(large_text.as_str()))
            .expect("Identity anchor must never be offloaded");
        assert_eq!(preserved.actual_tokens, large_text.len() as u32);
    }

    #[test]
    fn spill_never_offloads_working_memory_goal_state() {
        use crate::spill_backend::FileSystemSpillBackend;
        use tempfile::TempDir;

        let (plan, bound, latches) = build_test_plan_and_bound();
        let limits = OptimizeLimits {
            allow_spill: true,
            ..Default::default()
        };
        let policy = ProviderCachePolicy::default();

        let mut bound = bound;
        let large_goal_state = format!(
            "## Working Memory\nGoal: keep the user objective visible\n{}",
            "decision: preserve intent\n".repeat(20_000)
        );
        bound.sections.push(test_bound_section(
            SectionKind::WorkingMemory,
            CacheScope::None,
            &large_goal_state,
        ));

        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let result =
            optimize_with_spill(&plan, bound, &latches, &policy, &limits, 1, Some(&backend));

        assert_eq!(result.stats.entries_spilled, 0);
        assert!(result.spilled.is_empty());
        assert!(
            result
                .sections
                .iter()
                .any(|s| s.plan.kind == SectionKind::WorkingMemory
                    && s.text() == Some(large_goal_state.as_str())),
            "working memory carries goal continuity and must remain inline even under spill pressure"
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

    // ── rehydrate_sections (Phase 12: consumer side) ───────────────────

    #[test]
    fn rehydrate_sections_restores_spilled_section_content() {
        use crate::spill_backend::{
            FileSystemSpillBackend, SpillBackend, SpillRegistry, DEFAULT_SCHEME,
        };
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let backend: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let payload = b"ORIGINAL ProjectContext body".to_vec();
        let locator = backend.store("ProjectContext", &payload).unwrap();

        let mut sections = vec![BoundSection {
            plan: PlannedSection {
                kind: SectionKind::ProjectContext,
                scope: CacheScope::None,
                estimated_tokens: 100,
                priority: CompressionPriority::Normal,
                source: SectionSource::Static,
            },
            artifact: SectionArtifact::SpillReference {
                path: locator,
                original_tokens: 100,
            },
            actual_tokens: 0,
            bind_latency: std::time::Duration::ZERO,
        }];

        let mut reg = SpillRegistry::new();
        reg.register(DEFAULT_SCHEME, backend);

        let stats = rehydrate_sections(&mut sections, &reg);
        assert!(
            stats.skipped.is_empty(),
            "happy-path rehydrate must not record skipped entries"
        );
        assert!(matches!(
            sections[0].artifact,
            SectionArtifact::RuntimeText(_)
        ));
        assert_eq!(sections[0].text().unwrap(), "ORIGINAL ProjectContext body");
        assert!(sections[0].actual_tokens > 0);
    }

    #[test]
    fn rehydrate_sections_records_trace_on_load_error() {
        use crate::spill_backend::SpillRegistry;

        // Registry with NO backends registered → load will fail.
        let reg = SpillRegistry::new();
        let mut sections = vec![BoundSection {
            plan: PlannedSection {
                kind: SectionKind::Memory,
                scope: CacheScope::Session,
                estimated_tokens: 50,
                priority: CompressionPriority::Normal,
                source: SectionSource::Memory,
            },
            artifact: SectionArtifact::SpillReference {
                path: "file:///missing".into(),
                original_tokens: 50,
            },
            actual_tokens: 0,
            bind_latency: std::time::Duration::ZERO,
        }];

        let stats = rehydrate_sections(&mut sections, &reg);
        assert_eq!(stats.skipped.len(), 1);
        assert_eq!(stats.skipped[0].step, "rehydrate");
        // The section must REMAIN a SpillReference so callers can retry
        // later instead of having a placeholder burned in.
        assert!(matches!(
            sections[0].artifact,
            SectionArtifact::SpillReference { .. }
        ));
    }

    #[test]
    fn rehydrate_sections_is_noop_for_inline_artifacts() {
        use crate::spill_backend::SpillRegistry;

        let reg = SpillRegistry::new();
        let mut sections = vec![BoundSection {
            plan: PlannedSection {
                kind: SectionKind::Identity,
                scope: CacheScope::Global,
                estimated_tokens: 10,
                priority: CompressionPriority::Never,
                source: SectionSource::Static,
            },
            artifact: SectionArtifact::SystemText("core rules".into()),
            actual_tokens: 10,
            bind_latency: std::time::Duration::ZERO,
        }];

        let stats = rehydrate_sections(&mut sections, &reg);
        assert!(stats.skipped.is_empty());
        assert_eq!(sections[0].text().unwrap(), "core rules");
        assert_eq!(sections[0].actual_tokens, 10);
    }

    // ── Optimizer invariant proptests ───────────────────────────────────
    //
    // These lock invariants that the optimizer MUST uphold for any input:
    //  1. `cache_align_sections` never repositions Identity / Constraints.
    //  2. After a successful reorder, scope order is non-decreasing within
    //     the reorderable group.
    //  3. `compact_tool_results_gated` never exceeds `max_clear_tokens`.
    //  4. `spill_oversized_sections` never touches Identity / Constraints /
    //     WorkingMemory, regardless of size.
    //
    // Example tests around these invariants exist above, but the single-
    // example form can only prove the invariant for the handful of cases the
    // author thought of. proptest is the tripwire for regressions where a
    // future refactor passes the example tests but breaks on some corner
    // input distribution.
    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::sync::Mutex;

        /// In-memory spill backend for proptest — avoids touching the
        /// filesystem inside a property-based test (the 256+ invocations
        /// `proptest` will do against the `spill_never_touches_*` property
        /// would otherwise hammer /tmp pointlessly). `store` always
        /// succeeds; `load` is unused here because the property asserts
        /// *which* sections were spilled, not round-trip correctness.
        #[derive(Default)]
        struct InMemorySpillBackend {
            counter: Mutex<u64>,
        }

        impl InMemorySpillBackend {
            fn new() -> Self {
                Self::default()
            }
        }

        impl crate::spill_backend::SpillBackend for InMemorySpillBackend {
            fn store(&self, key_hint: &str, _bytes: &[u8]) -> std::io::Result<String> {
                let mut n = self.counter.lock().unwrap();
                *n += 1;
                Ok(format!("memory://{key_hint}-{}", *n))
            }
        }

        fn scope_strategy() -> impl Strategy<Value = CacheScope> {
            prop_oneof![
                Just(CacheScope::Global),
                Just(CacheScope::Session),
                Just(CacheScope::None),
            ]
        }

        fn kind_strategy() -> impl Strategy<Value = SectionKind> {
            // Mix anchors (Identity/Constraints/WorkingMemory) and regular
            // kinds so invariants that exclude them get exercised.
            prop_oneof![
                Just(SectionKind::Identity),
                Just(SectionKind::Constraints),
                Just(SectionKind::WorkingMemory),
                Just(SectionKind::SelfModel),
                Just(SectionKind::ProjectContext),
                Just(SectionKind::Memory),
                Just(SectionKind::Skills),
                Just(SectionKind::RuntimeIdentity),
                Just(SectionKind::RuntimeVolatile),
                Just(SectionKind::EmergentSkills),
                Just(SectionKind::EmergentMemory),
                Just(SectionKind::EmergentSummary),
            ]
        }

        fn arbitrary_bound_section(
            kind: SectionKind,
            scope: CacheScope,
            tokens: u32,
        ) -> BoundSection {
            // Pad text so `actual_tokens` is plausibly derived from it;
            // spill thresholds are in tokens so content length matters.
            let text = "x".repeat((tokens as usize).saturating_mul(4));
            BoundSection {
                plan: PlannedSection {
                    kind,
                    scope,
                    estimated_tokens: tokens,
                    priority: CompressionPriority::Normal,
                    source: SectionSource::Static,
                },
                artifact: SectionArtifact::from_text(kind, text),
                actual_tokens: tokens,
                bind_latency: std::time::Duration::ZERO,
            }
        }

        proptest! {
            /// Invariant 1: anchor sections (Identity, Constraints) MUST NOT
            /// move under `cache_align_sections`, regardless of scope or
            /// `max_moves` budget. Moving them invalidates the Anthropic
            /// prompt-cache prefix and breaks semantic ordering.
            #[test]
            fn reorder_never_moves_identity_or_constraints(
                kinds in prop::collection::vec(kind_strategy(), 0..12),
                scopes in prop::collection::vec(scope_strategy(), 0..12),
                max_moves in 0u32..=20,
            ) {
                let n = kinds.len().min(scopes.len());
                if n == 0 { return Ok(()); }
                let mut sections: Vec<BoundSection> = (0..n)
                    .map(|i| arbitrary_bound_section(kinds[i], scopes[i], (i as u32) + 1))
                    .collect();

                // Record original positions of every anchor and its text.
                let anchor_signatures_before: Vec<(usize, SectionKind, Option<String>)> = sections
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| matches!(
                        s.plan.kind,
                        SectionKind::Identity | SectionKind::Constraints
                    ))
                    .map(|(i, s)| (i, s.plan.kind, s.text().map(String::from)))
                    .collect();

                let _moves = cache_align_sections(&mut sections, max_moves);

                for (original_idx, original_kind, original_text) in anchor_signatures_before {
                    let section = &sections[original_idx];
                    prop_assert_eq!(
                        section.plan.kind,
                        original_kind,
                        "anchor at index {} changed kind: reorder must not displace \
                         Identity or Constraints",
                        original_idx
                    );
                    prop_assert_eq!(
                        section.text().map(String::from),
                        original_text,
                        "anchor content at index {} changed — reorder must not swap \
                         anchor payloads even if slot kind matched",
                        original_idx
                    );
                }
            }

            /// Invariant 2: when `cache_align_sections` applies its reorder,
            /// the reorderable group (everything except anchors) must end up
            /// with non-decreasing scope order (Global < Session < None). If
            /// it doesn't apply (moves > max_moves), the original order is
            /// preserved — but this test only checks the "applied" case.
            #[test]
            fn reorder_yields_non_decreasing_scope_when_applied(
                kinds in prop::collection::vec(kind_strategy(), 1..10),
                scopes in prop::collection::vec(scope_strategy(), 1..10),
            ) {
                let n = kinds.len().min(scopes.len());
                let mut sections: Vec<BoundSection> = (0..n)
                    .map(|i| arbitrary_bound_section(kinds[i], scopes[i], (i as u32) + 1))
                    .collect();
                // Generous max_moves so reorder always applies.
                let _moves = cache_align_sections(&mut sections, 1_000);

                let reorderable_scopes: Vec<u8> = sections
                    .iter()
                    .filter(|s| !matches!(
                        s.plan.kind,
                        SectionKind::Identity | SectionKind::Constraints
                    ))
                    .map(|s| s.plan.scope.order())
                    .collect();

                for pair in reorderable_scopes.windows(2) {
                    prop_assert!(
                        pair[0] <= pair[1],
                        "reorderable scope order is not non-decreasing: \
                         got {:?}, violates Global<Session<None",
                        reorderable_scopes
                    );
                }
            }

            /// Invariant 3: `compact_tool_results_gated` returns
            /// `tokens <= max_clear_tokens` (and `skipped_over_budget=true`
            /// when any would-be clearing exceeds the budget). This guards
            /// the over-compaction safety valve.
            #[test]
            fn tool_result_clearing_respects_max_clear_tokens(
                n_calls in 0usize..6,
                content_len in 0usize..200,
                max_clear_tokens in 0u32..=5_000,
                pressure_bits in 0u32..=100,
            ) {
                let pressure = f64::from(pressure_bits) / 100.0;
                let messages = {
                    if n_calls == 0 {
                        Vec::new()
                    } else {
                        let mut msgs = vec![serde_json::json!({
                            "role": "assistant",
                            "tool_calls": (0..n_calls)
                                .map(|i| serde_json::json!({
                                    "id": format!("call_{i}"),
                                    "function": {"name": "read_file", "arguments": "{}"}
                                }))
                                .collect::<Vec<_>>()
                        })];
                        for i in 0..n_calls {
                            msgs.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": format!("call_{i}"),
                                "content": "y".repeat(content_len),
                            }));
                        }
                        msgs
                    }
                };
                let mut msgs = messages.clone();
                let result = compact_tool_results_gated(
                    &mut msgs,
                    CompactionTier::CompactHistory,
                    pressure,
                    CompactStrategy::Minimal,
                    max_clear_tokens,
                );
                prop_assert!(
                    result.tokens <= max_clear_tokens,
                    "compact_tool_results_gated cleared {} tokens > cap {}",
                    result.tokens,
                    max_clear_tokens,
                );
                if result.skipped_over_budget {
                    prop_assert_eq!(
                        result.tokens, 0,
                        "skipped_over_budget must imply zero clearing"
                    );
                    prop_assert_eq!(
                        result.count, 0,
                        "skipped_over_budget must imply zero results counted"
                    );
                }
            }

            /// Invariant 4: `spill_oversized_sections` never replaces the
            /// artifact of Identity / Constraints / WorkingMemory even when
            /// they are oversized and a backend is available. Spilling these
            /// would hide the state needed to resume correctly after
            /// compaction and is explicitly prevented by the optimizer.
            #[test]
            fn spill_never_touches_anchor_or_working_memory(
                oversized_tokens in 10_001u32..50_000,
                include_identity in proptest::bool::ANY,
                include_constraints in proptest::bool::ANY,
                include_working in proptest::bool::ANY,
                include_regular in proptest::bool::ANY,
            ) {
                let mut sections: Vec<BoundSection> = Vec::new();
                if include_identity {
                    sections.push(arbitrary_bound_section(
                        SectionKind::Identity, CacheScope::Global, oversized_tokens));
                }
                if include_constraints {
                    sections.push(arbitrary_bound_section(
                        SectionKind::Constraints, CacheScope::Global, oversized_tokens));
                }
                if include_working {
                    sections.push(arbitrary_bound_section(
                        SectionKind::WorkingMemory, CacheScope::None, oversized_tokens));
                }
                if include_regular {
                    sections.push(arbitrary_bound_section(
                        SectionKind::Memory, CacheScope::None, oversized_tokens));
                }
                if sections.is_empty() { return Ok(()); }

                // Snapshot anchor/working-memory artifacts before spill.
                let protected_before: Vec<(usize, SectionKind, SectionArtifact)> = sections
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| matches!(
                        s.plan.kind,
                        SectionKind::Identity
                            | SectionKind::Constraints
                            | SectionKind::WorkingMemory
                    ))
                    .map(|(i, s)| (i, s.plan.kind, s.artifact.clone()))
                    .collect();

                let backend = InMemorySpillBackend::new();
                let mut stats = OptimizeStats::default();
                let plan = {
                    let (plan, _, _) = build_test_plan_and_bound();
                    plan
                };
                let _spilled = spill_oversized_sections(
                    &mut sections,
                    &mut stats,
                    Some(&backend),
                    &plan,
                    1,
                );

                for (idx, kind, original_artifact) in protected_before {
                    prop_assert!(
                        !matches!(sections[idx].artifact, SectionArtifact::SpillReference { .. }),
                        "{:?} at index {} was spilled despite being a protected section",
                        kind,
                        idx
                    );
                    // Protected sections must keep their original artifact bytes.
                    prop_assert_eq!(
                        sections[idx].artifact.text().map(String::from),
                        original_artifact.text().map(String::from),
                        "{:?} at index {} had its artifact text mutated by spill",
                        kind,
                        idx
                    );
                }
            }

            /// Invariant 5: `place_cache_markers` respects max_markers cap,
            /// never references out-of-bounds indices, always returns empty
            /// for Prefix protocol, and suppresses None-scope markers when
            /// a latch flipped this turn.
            #[test]
            fn cache_markers_respect_policy_and_latch_constraints(
                kinds in prop::collection::vec(kind_strategy(), 1..10),
                scopes in prop::collection::vec(scope_strategy(), 1..10),
                max_markers in 0u32..=6,
                use_anthropic in proptest::bool::ANY,
                latch_this_turn in proptest::bool::ANY,
                current_turn in 0u32..=100,
            ) {
                let n = kinds.len().min(scopes.len());
                let sections: Vec<BoundSection> = (0..n)
                    .map(|i| arbitrary_bound_section(kinds[i], scopes[i], (i as u32 + 1) * 10))
                    .collect();

                let policy = if use_anthropic {
                    ProviderCachePolicy {
                        protocol: PromptCacheProtocol::AnthropicCacheControl,
                        max_markers,
                        ..ProviderCachePolicy::anthropic()
                    }
                } else {
                    ProviderCachePolicy::openai_compatible()
                };

                let mut latches = SessionLatches::default();
                if latch_this_turn {
                    latches.latch_feature("test_feature", current_turn);
                }

                let markers = place_cache_markers(&sections, &policy, &latches, current_turn);

                // P1: Prefix protocol → always empty
                if policy.protocol == PromptCacheProtocol::Prefix {
                    prop_assert!(
                        markers.is_empty(),
                        "Prefix protocol must never emit markers"
                    );
                    return Ok(());
                }

                // P2: Never exceeds max_markers
                prop_assert!(
                    markers.len() <= max_markers as usize,
                    "markers.len()={} exceeds max_markers={}",
                    markers.len(),
                    max_markers
                );

                // P3: All indices are valid
                for marker in &markers {
                    prop_assert!(
                        marker.after_section_index < sections.len(),
                        "marker index {} >= sections.len() {}",
                        marker.after_section_index,
                        sections.len()
                    );
                }

                // P4: When latch flipped, no marker sits just before a None-scope section
                if latch_this_turn {
                    for marker in &markers {
                        let next_idx = marker.after_section_index + 1;
                        if next_idx < sections.len() {
                            prop_assert!(
                                sections[next_idx].plan.scope != CacheScope::None,
                                "latch flipped: marker at {} precedes None-scope section at {}",
                                marker.after_section_index,
                                next_idx
                            );
                        }
                    }
                }
            }
        }
    }
}
