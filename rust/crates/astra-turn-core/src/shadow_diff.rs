//! Shadow pipeline diff — compare old and new assembly outputs for safe rollout.
//!
//! The shadow runner executes both paths in parallel and diffs their outputs.
//! Any divergence produces a TraceAlert. This is the mechanism that makes
//! optimizer changes safe to iterate on.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::context_optimizer::ContextOptimized;
use crate::trace_alert::{AlertSeverity, TraceAlert};

/// Result of comparing two pipeline outputs.
#[derive(Debug, Default)]
pub struct ShadowDiffResult {
    pub alerts: Vec<TraceAlert>,
    pub section_count_match: bool,
    pub message_count_match: bool,
    pub tool_schema_count_match: bool,
    pub system_block_hash_match: bool,
}

/// Compare two ContextOptimized outputs (old path vs new pipeline).
pub fn diff_pipeline_outputs(
    old: &ContextOptimized,
    new: &ContextOptimized,
    turn: u32,
) -> ShadowDiffResult {
    let mut alerts = Vec::new();

    let section_count_match = old.sections.len() == new.sections.len();
    if !section_count_match {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Error,
            rule: "shadow_section_count".into(),
            message: format!(
                "Section count mismatch: old={}, new={}",
                old.sections.len(),
                new.sections.len(),
            ),
            turn,
        });
    }

    let message_count_match = old.messages.len() == new.messages.len();
    if !message_count_match {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Error,
            rule: "shadow_message_count".into(),
            message: format!(
                "Message count mismatch: old={}, new={}",
                old.messages.len(),
                new.messages.len(),
            ),
            turn,
        });
    }

    let tool_schema_count_match = old.tool_schemas.len() == new.tool_schemas.len();
    if !tool_schema_count_match {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Error,
            rule: "shadow_tool_schema_count".into(),
            message: format!(
                "Tool schema count mismatch: old={}, new={}",
                old.tool_schemas.len(),
                new.tool_schemas.len(),
            ),
            turn,
        });
    }

    let old_hash = hash_sections(&old.sections);
    let new_hash = hash_sections(&new.sections);
    let system_block_hash_match = old_hash == new_hash;
    if !system_block_hash_match {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Error,
            rule: "shadow_system_block_hash".into(),
            message: format!(
                "System block hash mismatch: old={old_hash:#x}, new={new_hash:#x}. Cache prefix would diverge.",
            ),
            turn,
        });
    }

    let old_tokens: u32 = old.sections.iter().map(|s| s.actual_tokens).sum();
    let new_tokens: u32 = new.sections.iter().map(|s| s.actual_tokens).sum();
    if old_tokens > 0 {
        let delta_pct = ((new_tokens as f64 - old_tokens as f64) / old_tokens as f64).abs();
        if delta_pct > 0.05 {
            alerts.push(TraceAlert {
                severity: AlertSeverity::Warning,
                rule: "shadow_token_estimate_delta".into(),
                message: format!(
                    "Token estimate delta {:.1}%: old={old_tokens}, new={new_tokens}",
                    delta_pct * 100.0,
                ),
                turn,
            });
        }
    }

    ShadowDiffResult {
        alerts,
        section_count_match,
        message_count_match,
        tool_schema_count_match,
        system_block_hash_match,
    }
}

fn hash_sections(sections: &[crate::section_types::BoundSection]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for section in sections {
        section.content.hash(&mut hasher);
        (section.plan.kind as u8).hash(&mut hasher);
    }
    hasher.finish()
}

/// Check if a shadow diff result is clean (no errors).
impl ShadowDiffResult {
    pub fn is_clean(&self) -> bool {
        self.alerts.iter().all(|a| a.severity < AlertSeverity::Error)
    }

    pub fn has_errors(&self) -> bool {
        self.alerts.iter().any(|a| a.severity >= AlertSeverity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_binder::bind_all;
    use crate::context_optimizer::optimize;
    use crate::context_planner::{plan_turn, PlanInput};
    use crate::context_sources::*;
    use crate::emergent_context::EmergentContext;
    use crate::microcompact::ProviderCacheStrategy;
    use crate::optimize_limits::OptimizeLimits;
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::pipeline_stats::PipelineStats;
    use crate::recovery_state::RecoveryState;
    use crate::session_latches::SessionLatches;
    use crate::token_accounting::TokenAccounting;
    use std::collections::HashMap;

    fn build_optimized() -> (ContextOptimized, SessionLatches) {
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
            project_context: "test".into(),
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
            has_memoria: false,
            spill_dir: None,
            has_fork_prefix: false,
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
            has_memoria: false,
            has_fork_prefix: false,
            model_id: "m1",
            query_source: "repl",
        };
        let plan = plan_turn(&plan_input);
        let bound = bind_all(&plan, &sources);
        let limits = OptimizeLimits::default();
        let policy = ProviderCachePolicy::default();
        let optimized = optimize(&plan, bound, &latches, &policy, &limits);
        (optimized, latches)
    }

    #[test]
    fn identical_outputs_no_alert() {
        let (opt1, _) = build_optimized();
        let (opt2, _) = build_optimized();
        let result = diff_pipeline_outputs(&opt1, &opt2, 1);
        assert!(result.is_clean(), "identical outputs should produce no errors: {:?}", result.alerts);
    }

    #[test]
    fn message_count_mismatch_emits_error() {
        let (mut opt1, _) = build_optimized();
        let (opt2, _) = build_optimized();
        opt1.messages.push(serde_json::json!({"role": "user", "content": "extra"}));
        let result = diff_pipeline_outputs(&opt1, &opt2, 2);
        assert!(result.has_errors());
        assert!(result.alerts.iter().any(|a| a.rule == "shadow_message_count"));
    }

    #[test]
    fn byte_hash_mismatch_emits_error() {
        let (mut opt1, _) = build_optimized();
        let (opt2, _) = build_optimized();
        if let Some(section) = opt1.sections.first_mut() {
            section.content.push_str(" extra content");
        }
        let result = diff_pipeline_outputs(&opt1, &opt2, 3);
        assert!(result.has_errors());
        assert!(result.alerts.iter().any(|a| a.rule == "shadow_system_block_hash"));
    }

    #[test]
    fn token_estimate_within_5pct_no_alert() {
        let (opt1, _) = build_optimized();
        let (opt2, _) = build_optimized();
        // Identical sections → identical tokens → no delta alert
        let result = diff_pipeline_outputs(&opt1, &opt2, 4);
        assert!(!result.alerts.iter().any(|a| a.rule == "shadow_token_estimate_delta"));
    }
}
