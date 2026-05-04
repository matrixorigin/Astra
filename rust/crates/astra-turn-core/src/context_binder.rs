//! Context pipeline Bind phase — resolve planned sections into concrete content.
//!
//! Each `bind_*` function takes a `PlannedSection` + relevant source data
//! and returns a `BoundSection`. In-memory bindings are sync; external
//! bindings (Memoria, disk) are async.

use std::time::Instant;

use serde_json::Value;

use crate::context_planner::ContextPlan;
use crate::context_sources::ContextSources;
use crate::section_types::{BoundSection, PlannedSection, SectionKind};

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
        SectionKind::History => bind_history(sources),
        SectionKind::RuntimeIdentity => bind_runtime_identity(sources),
        SectionKind::EmergentSkills => bind_emergent_skills(sources),
        SectionKind::EmergentMemory => bind_emergent_memory(sources),
        SectionKind::EmergentSummary => bind_emergent_summary(sources),
    };
    let actual_tokens = estimate_tokens(&content);
    let latency = start.elapsed();

    BoundSection {
        plan: planned.clone(),
        content,
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

/// Bind memory — placeholder for Memoria retrieval.
/// In production, this would be async and call MemoriaClient::retrieve().
fn bind_memory(_sources: &ContextSources<'_>) -> String {
    // TODO: Wire to actual Memoria retrieval in Phase 6
    String::new()
}

/// Bind conversation history as a text summary (token estimate).
fn bind_history(sources: &ContextSources<'_>) -> String {
    if sources.turn.messages.is_empty() {
        return String::new();
    }
    format!("[{} messages in history]", sources.turn.messages.len())
}

/// Bind runtime identity from edge profile.
fn bind_runtime_identity(sources: &ContextSources<'_>) -> String {
    let ep = &sources.session.edge_profile;
    let mut parts = Vec::new();
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
    (s.len() / 4) as u32
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

    fn test_sources() -> (
        StaticSections,
        AgentContext,
        SessionLatches,
        SessionContext,
        TurnState,
        ExternalSources,
        EmergentContext,
        PipelineStats,
    ) {
        (
            StaticSections::test_default(),
            AgentContext::default(),
            SessionLatches::default(),
            SessionContext {
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
            TurnState {
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
            ExternalSources {
                has_memoria: true,
                spill_dir: None,
                has_fork_prefix: false,
            },
            EmergentContext::default(),
            PipelineStats::default(),
        )
    }

    fn make_context_sources<'a>(
        statics: &'a StaticSections,
        agent: &'a AgentContext,
        latches: &'a SessionLatches,
        session: &'a SessionContext,
        turn: &'a TurnState,
        external: &'a ExternalSources,
        emergent: &'a EmergentContext,
        stats: &'a PipelineStats,
    ) -> ContextSources<'a> {
        ContextSources {
            statics,
            agent,
            latches,
            session,
            turn,
            external,
            emergent,
            stats,
        }
    }

    #[test]
    fn bind_identity_produces_global_scope_content() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_identity(&sources);
        assert!(content.contains("expert"), "identity should contain core rules");
        assert!(content.contains("Plan carefully"), "identity should contain planning");
    }

    #[test]
    fn bind_constraints_produces_global_scope_content() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_constraints(&sources);
        assert!(content.contains("concise"), "constraints should contain output format");
        assert!(content.contains("Retry"), "constraints should contain error recovery");
    }

    #[test]
    fn bind_runtime_identity_produces_none_scope() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_runtime_identity(&sources);
        assert!(content.contains("test-model"));
        assert!(content.contains("main")); // git branch
    }

    #[test]
    fn bind_history_shows_message_count() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_history(&sources);
        assert!(content.contains("1 messages"));
    }

    #[test]
    fn bind_skills_lists_active_skills() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_skills(&sources);
        assert!(content.contains("code_review"));
    }

    #[test]
    fn bind_emergent_skills_empty_when_no_discoveries() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_emergent_skills(&sources);
        assert!(content.is_empty());
    }

    #[test]
    fn bind_emergent_skills_lists_discovered() {
        let (statics, agent, latches, session, turn, ext, mut emer, stats) = test_sources();
        emer.push_skill(EmergentItem {
            value: DiscoveredSkill {
                skill_name: "review".into(),
                trigger: "file write".into(),
            },
            created_at_turn: 1,
            content_hash: 42,
        });
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);
        let content = bind_emergent_skills(&sources);
        assert!(content.contains("review"));
    }

    #[test]
    fn bind_all_produces_sections_matching_plan() {
        let (statics, agent, latches, session, turn, ext, emer, stats) = test_sources();
        let sources = make_context_sources(&statics, &agent, &latches, &session, &turn, &ext, &emer, &stats);

        let plan_input = crate::context_planner::PlanInput {
            tokens: &sources.turn.tokens,
            model_limit: 100_000,
            recovery: &sources.turn.recovery,
            latches: sources.latches,
            stats: sources.stats,
            provider_policy: &sources.session.provider_policy,
            has_memoria: sources.external.has_memoria,
            has_fork_prefix: sources.external.has_fork_prefix,
            model_id: &sources.session.model_id,
            query_source: "repl",
        };
        let plan = plan_turn(&plan_input);
        let bound = bind_all(&plan, &sources);

        assert_eq!(bound.sections.len(), plan.sections.len());
        assert!(!bound.messages.is_empty());
    }
}
