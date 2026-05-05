//! Context pipeline Bind phase — resolve planned sections into concrete content.
//!
//! Each `bind_*` function takes a `PlannedSection` + relevant source data
//! and returns a `BoundSection`. In-memory bindings are sync; external
//! bindings (Memoria, disk) are async.

use std::time::Instant;

use serde_json::Value;

use crate::context_planner::ContextPlan;
use crate::context_sources::ContextSources;
use crate::section_types::{
    BoundSection, PlannedSection, SectionArtifact, SectionKind, estimate_text_tokens,
};

/// Result of the Bind phase.
#[derive(Debug)]
pub struct ContextBound {
    pub sections: Vec<BoundSection>,
    pub messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
}

/// Execute the full Bind phase: resolve all planned sections into concrete content.
pub fn bind_all(plan: &ContextPlan, sources: &ContextSources<'_>) -> ContextBound {
    let mut sections = Vec::with_capacity(plan.sections.len());

    for planned in &plan.sections {
        let bound = bind_section(planned, sources);
        sections.push(bound);
    }

    // Messages come from TurnState
    let messages = sources.turn.messages.clone();

    // Tool schemas from AgentContext
    let tool_schemas = sources.agent.tool_schemas.clone();

    ContextBound {
        sections,
        messages,
        tool_schemas,
    }
}

/// Bind a single section based on its kind.
fn bind_section(planned: &PlannedSection, sources: &ContextSources<'_>) -> BoundSection {
    let start = Instant::now();
    let content = match planned.kind {
        SectionKind::Identity => bind_identity(sources),
        SectionKind::Constraints => bind_constraints(sources),
        SectionKind::SelfModel => bind_self_model(sources),
        SectionKind::ProjectContext => bind_project_context(sources),
        SectionKind::Skills => bind_skills(sources),
        SectionKind::Memory => bind_memory(sources),
        SectionKind::WorkingMemory => String::new(),
        // Conversation history is serialized as provider messages, not as a
        // separate text section.
        SectionKind::History => String::new(),
        SectionKind::RuntimeIdentity => bind_runtime_identity(sources),
        SectionKind::EmergentSkills => bind_emergent_skills(sources),
        SectionKind::EmergentMemory => bind_emergent_memory(sources),
        SectionKind::EmergentSummary => bind_emergent_summary(sources),
    };
    let actual_tokens = estimate_tokens(&content);
    let latency = start.elapsed();

    BoundSection {
        plan: planned.clone(),
        artifact: SectionArtifact::from_text(planned.kind, content),
        actual_tokens,
        bind_latency: latency,
    }
}

/// Bind identity section from static sections (Global scope).
fn bind_identity(sources: &ContextSources<'_>) -> String {
    let mut text = String::new();
    text.push_str(&sources.statics.core_rules.text);
    text.push('\n');
    text.push_str(&sources.statics.planning_protocol.text);
    text.push('\n');
    text.push_str(&sources.statics.coding_discipline.text);
    text.push('\n');
    text.push_str(&sources.statics.turn_discipline.text);
    text.push('\n');
    text.push_str(&sources.statics.parallel_efficiency.text);
    text
}

/// Bind constraints section (Global scope).
fn bind_constraints(sources: &ContextSources<'_>) -> String {
    let mut text = String::new();
    text.push_str(&sources.statics.output_format.text);
    text.push('\n');
    text.push_str(&sources.statics.tool_error_recovery.text);
    text
}

/// Bind self-model from session context.
fn bind_self_model(sources: &ContextSources<'_>) -> String {
    sources.session.self_model.clone().unwrap_or_default()
}

/// Bind project context from session.
fn bind_project_context(sources: &ContextSources<'_>) -> String {
    sources.session.project_context.clone()
}

/// Bind active skills.
fn bind_skills(sources: &ContextSources<'_>) -> String {
    if sources.turn.active_skills.is_empty() {
        return String::new();
    }
    format!("Active skills: {}", sources.turn.active_skills.join(", "))
}

/// Bind memory snippets that the runtime retrieved before entering core.
fn bind_memory(sources: &ContextSources<'_>) -> String {
    sources.external.memory_snippets.join("\n\n")
}

/// Bind runtime identity — the per-turn dynamic section.
///
/// Includes: model/env identity + all pre-computed dynamic fragments from
/// ExternalSources (profile, self-model, tool guidance, plan context, etc.).
/// The runtime computes these; the pipeline just includes them in order.
fn bind_runtime_identity(sources: &ContextSources<'_>) -> String {
    let ep = &sources.session.edge_profile;
    let ext = &sources.external;
    let mut parts = Vec::new();

    // Core identity (always present)
    parts.push(format!("Model: {}", sources.session.model_id));
    if let Some(cwd) = &ep.cwd {
        parts.push(format!("CWD: {cwd}"));
    }
    if let Some(branch) = &ep.git_branch {
        parts.push(format!("Branch: {branch}"));
    }
    if !sources.session.session_id.is_empty() {
        parts.push(format!("Session: {}", sources.session.session_id));
    }

    // Dynamic fragments from runtime (order matches legacy for cache stability)
    if let Some(ref text) = ext.self_model_text {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.tool_conditional {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.profile_desc {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.effort_hint {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.learned_context {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.system_override {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.plan_context {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.tool_guidance {
        parts.push(text.clone());
    }

    parts.join("\n")
}

/// Bind emergent skills from previous turn.
fn bind_emergent_skills(sources: &ContextSources<'_>) -> String {
    let skills = &sources.emergent.discovered_skills;
    if skills.is_empty() {
        return String::new();
    }
    let names: Vec<_> = skills.iter().map(|s| s.value.skill_name.as_str()).collect();
    format!("Discovered skills: {}", names.join(", "))
}

/// Bind emergent memory from previous turn.
fn bind_emergent_memory(sources: &ContextSources<'_>) -> String {
    let mems = &sources.emergent.prefetched_memory;
    if mems.is_empty() {
        return String::new();
    }
    mems.iter()
        .map(|m| m.value.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bind emergent tool use summary from previous turn.
fn bind_emergent_summary(sources: &ContextSources<'_>) -> String {
    sources
        .emergent
        .tool_summaries
        .first()
        .map(|s| s.value.summary.clone())
        .unwrap_or_default()
}

/// Rough token estimate: ~4 bytes per token.
fn estimate_tokens(s: &str) -> u32 {
    estimate_text_tokens(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_planner::plan_turn;
    use crate::context_sources::*;
    use crate::emergent_context::*;
    use crate::microcompact::ProviderCacheStrategy;
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::pipeline_stats::PipelineStats;
    use crate::recovery_state::RecoveryState;
    use crate::session_latches::SessionLatches;
    use crate::token_accounting::TokenAccounting;
    use std::collections::HashMap;

    struct TestSources {
        statics: StaticSections,
        agent: AgentContext,
        latches: SessionLatches,
        session: SessionContext,
        turn: TurnState,
        external: ExternalSources,
        emergent: EmergentContext,
        stats: PipelineStats,
    }

    impl TestSources {
        fn context(&self) -> ContextSources<'_> {
            ContextSources {
                statics: &self.statics,
                agent: &self.agent,
                latches: &self.latches,
                session: &self.session,
                turn: &self.turn,
                external: &self.external,
                emergent: &self.emergent,
                stats: &self.stats,
            }
        }
    }

    fn test_sources() -> TestSources {
        TestSources {
            statics: StaticSections::test_default(),
            agent: AgentContext::default(),
            latches: SessionLatches::default(),
            session: SessionContext {
                session_id: "test-session".into(),
                run_id: "test-run".into(),
                model_id: "test-model".into(),
                model_limit: 100_000,
                provider_policy: ProviderCachePolicy::default(),
                provider_strategy: ProviderCacheStrategy::default(),
                project_context: "Rust project".into(),
                edge_profile: EdgeProfile {
                    cwd: Some("/home/user/project".into()),
                    git_branch: Some("main".into()),
                    ..Default::default()
                },
                self_model: Some("Expert coder.".into()),
            },
            turn: TurnState {
                messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
                tool_results: vec![],
                tokens: TokenAccounting::default(),
                active_skills: vec!["code_review".into()],
                recent_file_reads: HashMap::new(),
                remaining_turns: 10,
                turn_index: 1,
                recovery: RecoveryState::default(),
                last_user_message: "hello".into(),
            },
            external: ExternalSources {
                memory_snippets: vec!["Remember: prefer pipeline-first design.".into()],
                spill_dir: None,
            ..Default::default()
            },
            emergent: EmergentContext::default(),
            stats: PipelineStats::default(),
        }
    }

    #[test]
    fn bind_identity_produces_global_scope_content() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_identity(&sources);
        assert!(
            content.contains("expert"),
            "identity should contain core rules"
        );
        assert!(
            content.contains("Plan carefully"),
            "identity should contain planning"
        );
    }

    #[test]
    fn bind_constraints_produces_global_scope_content() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_constraints(&sources);
        assert!(
            content.contains("concise"),
            "constraints should contain output format"
        );
        assert!(
            content.contains("Retry"),
            "constraints should contain error recovery"
        );
    }

    #[test]
    fn bind_runtime_identity_produces_none_scope() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_runtime_identity(&sources);
        assert!(content.contains("test-model"));
        assert!(content.contains("main")); // git branch
    }

    #[test]
    fn bind_memory_uses_retrieved_snippets() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_memory(&sources);
        assert!(content.contains("pipeline-first"));
    }

    #[test]
    fn bind_skills_lists_active_skills() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_skills(&sources);
        assert!(content.contains("code_review"));
    }

    #[test]
    fn bind_emergent_skills_empty_when_no_discoveries() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_emergent_skills(&sources);
        assert!(content.is_empty());
    }

    #[test]
    fn bind_emergent_skills_lists_discovered() {
        let mut fixture = test_sources();
        fixture.emergent.push_skill(EmergentItem {
            value: DiscoveredSkill {
                skill_name: "review".into(),
                trigger: "file write".into(),
            },
            created_at_turn: 1,
            content_hash: 42,
        });
        let sources = fixture.context();
        let content = bind_emergent_skills(&sources);
        assert!(content.contains("review"));
    }

    #[test]
    fn bind_all_produces_sections_matching_plan() {
        let fixture = test_sources();
        let sources = fixture.context();

        let plan_input = crate::context_planner::PlanInput {
            tokens: &sources.turn.tokens,
            model_limit: 100_000,
            recovery: &sources.turn.recovery,
            latches: sources.latches,
            stats: sources.stats,
            provider_policy: &sources.session.provider_policy,
            has_memory: !sources.external.memory_snippets.is_empty(),
            model_id: &sources.session.model_id,
            query_source: "repl",
        };
        let plan = plan_turn(&plan_input);
        let bound = bind_all(&plan, &sources);

        assert_eq!(bound.sections.len(), plan.sections.len());
        assert!(!bound.messages.is_empty());
    }
}
