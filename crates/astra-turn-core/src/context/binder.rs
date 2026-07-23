//! Context pipeline Bind phase — resolve planned sections into concrete content.
//!
//! Each `bind_*` function takes a `PlannedSection` + relevant source data
//! and returns a `BoundSection`. The runtime pre-fetches external data before
//! entering core, so binding is a pure in-memory transform.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Instant;

use serde_json::Value;

use crate::context_planner::ContextPlan;
use crate::context_sources::{ContextSources, MemoryEntry};
use crate::section_types::{
    BYTES_PER_TOKEN_ESTIMATE, BoundSection, PlannedSection, SectionArtifact, SectionKind,
    estimate_text_tokens,
};
use crate::working_memory::WorkingMemoryState;

const MEMORY_SECTION_HEADER: &str = "## User Memories\n";

/// Result of the Bind phase.
#[derive(Debug)]
pub struct ContextBound {
    pub sections: Vec<BoundSection>,
    pub messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
}

/// Execute the full Bind phase: resolve all planned sections into concrete content.
pub fn bind_all(plan: &ContextPlan, sources: &ContextSources<'_>) -> ContextBound {
    let sections = bind_sections(plan, sources);

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

/// Bind only planned sections without cloning the conversation or tool
/// working sets. Used by bind-aware planning to measure a candidate before
/// the final wire view is materialized.
pub(crate) fn bind_sections(plan: &ContextPlan, sources: &ContextSources<'_>) -> Vec<BoundSection> {
    plan.sections
        .iter()
        .map(|planned| bind_section(planned, sources))
        .collect()
}

/// Bind a single section based on its kind.
fn bind_section(planned: &PlannedSection, sources: &ContextSources<'_>) -> BoundSection {
    let start = Instant::now();
    let content = match planned.kind {
        SectionKind::Identity => bind_identity(sources),
        SectionKind::Constraints => bind_constraints(sources),
        SectionKind::SelfModel => bind_self_model(sources),
        SectionKind::ProjectContext => bind_project_context(sources),
        SectionKind::DeferredTools => bind_deferred_tools(sources),
        SectionKind::AvailableSkills => bind_available_skills(sources),
        SectionKind::Skills => bind_skills(sources),
        SectionKind::Memory => bind_memory(planned, sources),
        SectionKind::WorkingMemory => bind_working_memory(sources),
        // Conversation history is serialized as provider messages, not as a
        // separate text section.
        SectionKind::History => String::new(),
        SectionKind::RuntimeIdentity => bind_runtime_identity(sources),
        SectionKind::RuntimeVolatile => bind_runtime_volatile(sources),
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
    text.push_str(&sources.statics.safety.text);
    text.push('\n');
    text.push_str(&sources.statics.planning_protocol.text);
    text.push('\n');
    text.push_str(&sources.statics.coding_discipline.text);
    text.push('\n');
    text.push_str(&sources.statics.turn_discipline.text);
    text.push('\n');
    text.push_str(&sources.statics.plan_execution.text);
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

/// Bind project-level session context — cross-session project summaries.
fn bind_project_context(sources: &ContextSources<'_>) -> String {
    let ctx = &sources.session.project_context;

    if ctx.is_empty() {
        return String::new();
    }
    format!(
        "## Cross-Session Project Context\n\
         Below are summaries of recent sessions in this project. \
         Use them for continuity — avoid re-asking questions already answered.\n\n\
         {ctx}"
    )
}

/// Bind the session-stable deferred-tools discovery block.
fn bind_deferred_tools(sources: &ContextSources<'_>) -> String {
    sources.session.deferred_tools_block.clone()
}

/// Bind the session-stable available-skills catalog block.
fn bind_available_skills(sources: &ContextSources<'_>) -> String {
    sources.session.skill_listing_block.clone()
}

/// Bind active skills.
fn bind_skills(sources: &ContextSources<'_>) -> String {
    if sources.turn.active_skills.is_empty() {
        return String::new();
    }
    format!("Active skills: {}", sources.turn.active_skills.join(", "))
}

/// Bind memory entries that the runtime retrieved before entering core.
fn bind_memory(planned: &PlannedSection, sources: &ContextSources<'_>) -> String {
    let header_tokens = estimate_text_tokens(MEMORY_SECTION_HEADER).max(1);
    let budget = planned.estimated_tokens.saturating_sub(header_tokens);
    if budget == 0 || sources.external.memory_entries.is_empty() {
        return String::new();
    }

    let mut entries = sources.external.memory_entries.clone();
    entries.sort_by(|a, b| {
        compare_score_desc(a.relevance_score, b.relevance_score)
            .then_with(|| b.freshness_turn.cmp(&a.freshness_turn))
            .then_with(|| a.content_hash.cmp(&b.content_hash))
    });

    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut used = 0_u32;
    for entry in entries {
        if used == budget {
            break;
        }
        if entry.content.trim().is_empty() || !seen.insert(entry.content_hash) {
            continue;
        }
        let prompt_content = render_prompt_memory_entry(&entry);
        let estimate = entry
            .token_estimate
            .max(estimate_text_tokens(&prompt_content))
            .max(1);
        let remaining = budget.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        if estimate <= remaining {
            used = used.saturating_add(estimate);
            selected.push(prompt_content);
        } else if selected.is_empty() {
            selected.push(truncate_memory_to_budget(&prompt_content, remaining));
            break;
        }
    }

    if selected.is_empty() {
        String::new()
    } else {
        format!("{MEMORY_SECTION_HEADER}{}", selected.join("\n\n"))
    }
}

fn render_prompt_memory_entry(entry: &MemoryEntry) -> String {
    match (
        entry
            .memory_id
            .as_deref()
            .filter(|id| !id.trim().is_empty()),
        entry
            .memory_type
            .as_deref()
            .filter(|kind| !kind.trim().is_empty()),
    ) {
        (Some(memory_id), Some(memory_type)) => format!(
            "[Memory evidence id={memory_id} type={memory_type}]\n{}",
            entry.content.trim()
        ),
        _ => entry.content.trim().to_string(),
    }
}

fn compare_score_desc(a: f64, b: f64) -> Ordering {
    let a = if a.is_finite() { a } else { f64::NEG_INFINITY };
    let b = if b.is_finite() { b } else { f64::NEG_INFINITY };
    b.partial_cmp(&a).unwrap_or(Ordering::Equal)
}

fn truncate_memory_to_budget(content: &str, budget_tokens: u32) -> String {
    if budget_tokens == 0 {
        return String::new();
    }
    let char_budget = (budget_tokens as usize).saturating_mul(BYTES_PER_TOKEN_ESTIMATE);
    if content.len() <= char_budget {
        return content.to_string();
    }
    let mut end = char_budget.min(content.len());
    while !content.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    content[..end].to_string()
}

/// Bind first-class working memory: goal, decisions, blockers, next action.
fn bind_working_memory(sources: &ContextSources<'_>) -> String {
    sources
        .working_memory
        .map(WorkingMemoryState::render_prompt_section)
        .unwrap_or_default()
}

/// Bind the **session-stable** runtime identity fragments.
///
/// Includes typed CWD/Branch header plus fragments that only
/// change at session boundaries: `system_override` and opt-in
/// `extra_stable_sections` (environment_static from the bridge / adapter
/// edge_profile, output style, etc.). Runtime facts whose placement depends on
/// provider cache semantics, including model identity, are injected by the
/// runtime entrypoint before binding. Turn-volatile content —
/// self-awareness, tool-dependent guidance, memoria insights — routes
/// through `RuntimeVolatile` (`bind_runtime_volatile`) so it sits after
/// the Session→None cache marker and doesn't invalidate the prefix.
/// These stable pieces sit in `CacheScope::Session` so Anthropic's
/// per-session cache captures them behind the 2nd cache marker.
///
/// NOTE: `session_id` is deliberately emitted in the **volatile** section
/// (`bind_runtime_volatile`) rather than here. Placing a per-session UUID
/// in the Session-scoped (cacheable) block breaks cross-session prefix
/// sharing — every new session would invalidate the cached prefix even
/// though the cwd/branch/tools are identical.
fn bind_runtime_identity(sources: &ContextSources<'_>) -> String {
    let ep = &sources.session.edge_profile;
    let ext = &sources.external;
    let mut parts = Vec::new();

    // Agent version: compile-time constant, truly session-stable.
    parts.push(format!("Astra v{}", env!("CARGO_PKG_VERSION")));

    // Current date (session-stable: computed once at session creation)
    let current_date = &sources.session.current_date;
    parts.push(format!("Date: {current_date}"));

    // Authenticated user (session-stable: set once at session creation)
    if let Some(ref uid) = sources.session.user_id {
        parts.push(format!("User: {uid}"));
    }

    if let Some(cwd) = &ep.cwd {
        parts.push(format!("CWD: {cwd}"));
    }
    if let Some(branch) = &ep.git_branch {
        parts.push(format!("Branch: {branch}"));
    }

    // Session-stable dynamic fragments. Order matches the legacy
    // `bind_runtime_identity` emission order for byte stability across
    // refactors.

    if let Some(ref text) = ext.system_override {
        parts.push(text.clone());
    }

    // Bridge stable escape hatch: session-stable pre-composed fragments
    // (skill_hint, self_awareness, etc.). Binder appends
    // them here so they inherit RuntimeIdentity's Session scope → cached
    // behind the 2nd marker like the typed fragments above.
    for section in &ext.extra_stable_sections {
        if !section.text.is_empty() {
            parts.push(section.text.clone());
        }
    }

    parts.join("\n")
}

/// Bind the **turn-volatile** runtime identity fragments.
///
/// Includes pieces that can change every turn: `effort_hint` (depends on
/// the active skill), `plan_context` (resume hint), `tool_guidance` (uses
/// current conversation length), and `extra_dynamic_sections` (bridge
/// escape hatch — session anchor, feedback rules, memoria insights that
/// update each turn). These sit in `CacheScope::None` so turn-to-turn
/// drift does not invalidate the cached session prefix.
fn bind_runtime_volatile(sources: &ContextSources<'_>) -> String {
    let ext = &sources.external;
    let mut parts = Vec::new();

    // Session UUID is *not* emitted to the prompt. It used to ride the
    // volatile lane ("Session: <uuid>\n" = ~45c/turn) so it wouldn't
    // invalidate the cached session prefix, but no prompt fragment
    // instructs the model to read or cite the UUID — it was pure
    // operator decoration for LLM-side logs. We already journal
    // session_id at the transport layer; paying ~45c every turn for a
    // value the model never references is net loss. If a downstream
    // audit mechanism ever needs it visible to the model, reintroduce
    // here (keep volatile-only, never stable).
    let _session_id_unused = &sources.session.session_id;

    if let Some(ref text) = ext.effort_hint {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.plan_context {
        parts.push(text.clone());
    }
    if let Some(ref text) = ext.tool_guidance {
        parts.push(text.clone());
    }
    if let Some(ref session_memory) = ext.session_memory_entry {
        if !session_memory.content.is_empty() {
            parts.push(session_memory.content.clone());
        }
    }

    // Bridge escape hatch: pre-built per-turn dynamic sections (session
    // anchor, feedback rules, memoria insights, etc.). Appended verbatim
    // in caller order so the bridge's legacy composition can be replicated
    // without adding 10 more typed fields.
    for section in &ext.extra_dynamic_sections {
        if !section.text.is_empty() {
            parts.push(section.text.clone());
        }
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
    use crate::working_memory::WorkingMemoryState;
    use std::collections::HashMap;

    struct TestSources {
        statics: StaticSections,
        agent: AgentContext,
        latches: SessionLatches,
        session: SessionContext,
        turn: TurnState,
        external: ExternalSources,
        emergent: EmergentContext,
        working_memory: WorkingMemoryState,
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
                working_memory: Some(&self.working_memory),
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
                provider_name: "anthropic".into(),
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
                deferred_tools_block: String::new(),
                skill_listing_block: String::new(),
                current_date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
                user_id: None,
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
                memory_entries: vec![MemoryEntry::new("Remember: prefer pipeline-first design.")],
                spill_dir: None,
                ..Default::default()
            },
            emergent: EmergentContext::default(),
            working_memory: WorkingMemoryState::default(),
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

    /// Project context from prior sessions must be rendered with the
    /// "Cross-Session Project Context" header wrapper so the wire payload
    /// matches the legacy runtime-injected version (pre-Phase-4 this was
    /// pushed as a raw `role: system` message with the same header from
    /// `agentic_loop/lifecycle.rs`).
    #[test]
    fn bind_project_context_wraps_in_cross_session_header() {
        let mut fixture = test_sources();
        fixture.session.project_context = "1. [active] (2026-05-06, 22 turns)".to_string();
        let sources = fixture.context();
        let content = bind_project_context(&sources);

        assert!(
            content.starts_with("## Cross-Session Project Context"),
            "bound content must lead with the stable header: {content}"
        );
        assert!(
            content.contains("Use them for continuity"),
            "bound content must include the guidance blurb so the LLM knows what the block is"
        );
        assert!(
            content.contains("22 turns"),
            "bound content must include the caller-provided summaries"
        );
    }

    #[test]
    fn bind_project_context_empty_when_no_summaries() {
        // No prior sessions → no section emitted → serializer drops the
        // empty block so no wasted bytes on the wire.
        let mut fixture = test_sources();
        fixture.session.project_context = String::new();
        let sources = fixture.context();
        let content = bind_project_context(&sources);
        assert!(
            content.is_empty(),
            "empty project_context must NOT render a bare header: {content}"
        );
    }

    #[test]
    fn bind_deferred_tools_returns_manifest_without_project_context_coupling() {
        let mut fixture = test_sources();
        fixture.session.project_context = "prior-session-summary-stub".to_string();
        fixture.session.deferred_tools_block =
            "<deferred-tools>\nweb_fetch\n</deferred-tools>".to_string();
        let sources = fixture.context();
        let project_context = bind_project_context(&sources);
        let content = bind_deferred_tools(&sources);

        assert!(
            !project_context.contains("<deferred-tools>"),
            "ProjectContext must not mix tool discovery into the same cache section"
        );
        assert!(
            content.contains("<deferred-tools>"),
            "deferred_tools section must preserve the manifest block; got:\n{content}"
        );
        assert!(
            content.contains("\nweb_fetch\n"),
            "entries from the block must be preserved verbatim"
        );
        assert!(
            !content.contains("<tool>")
                && !content.contains("<name>")
                && !content.contains("<description>"),
            "deferred tools must stay as a plain name list: {content}"
        );
    }

    #[test]
    fn bind_available_skills_returns_catalog_without_project_context_coupling() {
        let mut fixture = test_sources();
        fixture.session.project_context = "prior-session-summary-stub".to_string();
        fixture.session.skill_listing_block =
            "<available_skills>\n  <skill>\n    <name>markdown</name>\n    <description>Output Format</description>\n  </skill>\n</available_skills>".to_string();
        let sources = fixture.context();
        let project_context = bind_project_context(&sources);
        let content = bind_available_skills(&sources);

        assert!(
            !project_context.contains("<available_skills>"),
            "ProjectContext must not mix skill discovery into the same cache section"
        );
        assert!(
            content.contains("<available_skills>"),
            "available-skills section must preserve the catalog block; got:\n{content}"
        );
    }

    #[test]
    fn bind_all_keeps_project_deferred_and_skill_catalog_as_ordered_session_sections() {
        let mut fixture = test_sources();
        fixture.session.project_context = "prior-session-summary-stub".to_string();
        fixture.session.deferred_tools_block = "<deferred-tools>x</deferred-tools>".to_string();
        fixture.session.skill_listing_block = "<available_skills>y</available_skills>".to_string();
        let sources = fixture.context();
        let plan_input = crate::context_planner::PlanInput {
            tokens: &sources.turn.tokens,
            model_limit: 100_000,
            recovery: &sources.turn.recovery,
            latches: sources.latches,
            stats: sources.stats,
            provider_policy: &sources.session.provider_policy,
            has_memory: false,
            model_id: &sources.session.model_id,
            query_source: "repl",
        };
        let mut plan = plan_turn(&plan_input);
        plan.sections.retain(|section| {
            matches!(
                section.kind,
                SectionKind::ProjectContext
                    | SectionKind::DeferredTools
                    | SectionKind::AvailableSkills
            )
        });
        let bound = bind_all(&plan, &sources);

        let kinds: Vec<SectionKind> = bound
            .sections
            .iter()
            .map(|section| section.plan.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                SectionKind::ProjectContext,
                SectionKind::DeferredTools,
                SectionKind::AvailableSkills,
            ],
            "session-stable discovery blocks must remain independently traceable and ordered"
        );
        assert_eq!(
            bound.sections[0].artifact.text().unwrap(),
            "## Cross-Session Project Context\nBelow are summaries of recent sessions in this project. Use them for continuity — avoid re-asking questions already answered.\n\nprior-session-summary-stub"
        );
        assert_eq!(
            bound.sections[1].artifact.text().unwrap(),
            "<deferred-tools>x</deferred-tools>"
        );
        assert_eq!(
            bound.sections[2].artifact.text().unwrap(),
            "<available_skills>y</available_skills>"
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
    fn bind_runtime_identity_leaves_model_identity_to_runtime_policy() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_runtime_identity(&sources);
        assert!(
            !content.contains("Model: test-model"),
            "core binder must not decide model identity cache placement: {content}"
        );
        assert!(content.contains("main")); // git branch
    }

    #[test]
    fn bind_runtime_volatile_leaves_model_identity_to_runtime_policy() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_runtime_volatile(&sources);
        assert!(
            !content.contains("Model: test-model"),
            "runtime entrypoints inject model identity with provider-aware cache placement: {content}"
        );
    }

    /// HTTP bridge escape hatch: `extra_dynamic_sections` gives callers a
    /// way to feed pre-built sections (session anchor, feedback rules,
    /// memoria insights, etc.) through the pipeline without each one
    /// needing a dedicated typed field. Post-split these land in the
    /// volatile section (they can vary turn-to-turn — session anchors
    /// update, feedback rules grow, memoria insights rotate) and stay
    /// OUT of the session-cached prefix.
    #[test]
    fn bind_runtime_volatile_includes_extra_dynamic_sections() {
        use crate::section_types::PromptSection;
        use crate::section_types::PromptTokenBucket;

        let mut fixture = test_sources();
        fixture.external.extra_dynamic_sections = vec![
            PromptSection::dynamic(
                "\n\n## Session Anchor\nOriginal task: build a CLI.".to_string(),
                PromptTokenBucket::Environment,
            ),
            PromptSection::dynamic(
                "\n\n[Learned Feedback Rules]\n- No mocks in integration tests.".to_string(),
                PromptTokenBucket::Environment,
            ),
        ];

        let sources = fixture.context();
        let content = bind_runtime_volatile(&sources);

        assert!(
            content.contains("Session Anchor"),
            "first extra section must appear in runtime volatile: {content}"
        );
        assert!(
            content.contains("Learned Feedback Rules"),
            "second extra section must also appear: {content}"
        );
        // Order preserved — anchor before feedback rules
        let anchor_pos = content.find("Session Anchor").unwrap();
        let rules_pos = content.find("Learned Feedback Rules").unwrap();
        assert!(
            anchor_pos < rules_pos,
            "extra_dynamic_sections must append in caller-specified order"
        );

        // The session-stable RuntimeIdentity must NOT leak the extras —
        // otherwise the cached prefix would drift with bridge escape-hatch
        // churn.
        let stable = bind_runtime_identity(&sources);
        assert!(
            !stable.contains("Session Anchor"),
            "extras must stay out of session-stable identity — would break cache"
        );
    }

    #[test]
    fn bind_runtime_volatile_includes_session_memory_entry() {
        let mut fixture = test_sources();
        fixture.external.session_memory_entry = Some(MemoryEntry::new(
            "## Session State\nLatest state: implement pipeline-native session memory",
        ));

        let sources = fixture.context();
        let content = bind_runtime_volatile(&sources);

        assert!(
            content.contains("## Session State"),
            "session memory must be routed through runtime volatile: {content}"
        );
        assert!(
            content.contains("pipeline-native session memory"),
            "session memory content must survive binding: {content}"
        );

        let stable = bind_runtime_identity(&sources);
        assert!(
            !stable.contains("## Session State"),
            "session memory must stay out of session-stable identity: {stable}"
        );
    }

    /// The session UUID must NOT appear anywhere in either scope — it
    /// used to lead the volatile block ("Session: <uuid>") and cost
    /// ~45c every turn with no prompt fragment ever instructing the
    /// model to read or cite it. If this test fails because someone
    /// re-added it, confirm there is an actual prompt rule that
    /// references the UUID before reverting.
    #[test]
    fn bind_runtime_volatile_does_not_emit_session_id() {
        let fixture = test_sources();
        let sources = fixture.context();
        let volatile = bind_runtime_volatile(&sources);
        let stable = bind_runtime_identity(&sources);
        assert!(
            !volatile.contains(sources.session.session_id.as_str()),
            "session UUID must not appear in volatile prompt: {volatile}"
        );
        assert!(
            !stable.contains(sources.session.session_id.as_str()),
            "session UUID must not appear in stable prompt either: {stable}"
        );
        assert!(
            !volatile.starts_with("Session:"),
            "legacy 'Session: ...' line must be gone from volatile: {volatile}"
        );
    }

    #[test]
    fn bind_runtime_identity_empty_extras_behaves_like_before() {
        // Backward-compat: empty `extra_dynamic_sections` (the default)
        // must not change the session-stable output versus pre-refactor.
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_runtime_identity(&sources);
        // Core identity still intact; no spurious trailing content.
        assert!(content.contains("main"));
        assert!(
            !content.ends_with("\n\n"),
            "no orphan blank lines from empty extras"
        );
    }

    #[test]
    fn bind_memory_uses_retrieved_entries() {
        let fixture = test_sources();
        let sources = fixture.context();
        let planned = planned_memory(1024);
        let content = bind_memory(&planned, &sources);
        assert!(content.contains("pipeline-first"));
    }

    #[test]
    fn bind_memory_exposes_typed_identity_for_correction_and_feedback() {
        let mut fixture = test_sources();
        fixture.external.memory_entries = vec![
            MemoryEntry::scored("[@pref/active] Prefer the server-side execution path", 0.9)
                .with_memory_identity("mem-42", "profile"),
        ];
        let sources = fixture.context();

        let content = bind_memory(&planned_memory(128), &sources);

        assert!(content.contains("[Memory evidence id=mem-42 type=profile]"));
        assert!(content.contains("[@pref/active] Prefer the server-side execution path"));
    }

    #[test]
    fn bind_memory_ranks_deduplicates_and_respects_budget() {
        let mut fixture = test_sources();
        fixture.external.memory_entries = vec![
            MemoryEntry::scored("low relevance memory that should be dropped", 0.1),
            MemoryEntry::scored("high relevance memory", 0.9).with_freshness_turn(1),
            MemoryEntry::scored("high relevance memory", 0.9).with_freshness_turn(9),
            MemoryEntry::scored("medium relevance memory", 0.5),
        ];
        let planned = planned_memory(12);
        let sources = fixture.context();

        let content = bind_memory(&planned, &sources);

        assert!(
            content.starts_with("## User Memories\nhigh relevance memory"),
            "highest-scored memory should lead: {content}"
        );
        assert_eq!(
            content.matches("high relevance memory").count(),
            1,
            "duplicate memory content must be hash-deduped: {content}"
        );
        assert!(
            !content.contains("low relevance"),
            "low-score memory should be dropped first under budget: {content}"
        );
    }

    #[test]
    fn bind_memory_treats_nan_relevance_as_lowest() {
        let mut fixture = test_sources();
        fixture.external.memory_entries = vec![
            MemoryEntry::scored("nan relevance memory", f64::NAN),
            MemoryEntry::scored("finite relevance memory", 0.1),
        ];
        let planned = planned_memory(32);
        let sources = fixture.context();

        let content = bind_memory(&planned, &sources);

        assert!(
            content.starts_with("## User Memories\nfinite relevance memory"),
            "finite relevance should outrank NaN: {content}"
        );
    }

    #[test]
    fn bind_memory_zero_budget_is_empty_by_contract() {
        let fixture = test_sources();
        let sources = fixture.context();
        let content = bind_memory(&planned_memory(0), &sources);
        assert!(
            content.is_empty(),
            "planner must allocate non-zero memory budget when has_memory=true"
        );
    }

    #[test]
    fn bind_memory_truncates_single_oversized_entry_on_utf8_boundary() {
        let mut fixture = test_sources();
        fixture.external.memory_entries =
            vec![MemoryEntry::scored("ééé", 1.0).with_token_estimate(100)];
        let sources = fixture.context();

        let budget = estimate_text_tokens(MEMORY_SECTION_HEADER).max(1) + 1;
        let content = bind_memory(&planned_memory(budget), &sources);

        assert_eq!(content, "## User Memories\néé");
    }

    #[test]
    fn bind_memory_skips_empty_entries_before_budgeting() {
        let mut fixture = test_sources();
        fixture.external.memory_entries = vec![
            MemoryEntry::scored("   ", 10.0),
            MemoryEntry::scored("usable memory", 1.0),
        ];
        let sources = fixture.context();

        let content = bind_memory(&planned_memory(32), &sources);

        assert_eq!(content, "## User Memories\nusable memory");
    }

    #[test]
    fn bind_memory_uses_freshness_then_hash_tiebreaks() {
        let older = MemoryEntry::scored("older memory", 1.0).with_freshness_turn(1);
        let newer = MemoryEntry::scored("newer memory", 1.0).with_freshness_turn(9);
        assert_ne!(older.content_hash, newer.content_hash);

        let mut fixture = test_sources();
        fixture.external.memory_entries = vec![older, newer];
        let sources = fixture.context();
        let content = bind_memory(&planned_memory(64), &sources);

        assert!(
            content.starts_with("## User Memories\nnewer memory"),
            "freshness must sort descending before hash tiebreak: {content}"
        );

        let alpha = MemoryEntry::scored("alpha", 1.0);
        let beta = MemoryEntry::scored("beta", 1.0);
        let expected_first = if alpha.content_hash < beta.content_hash {
            "alpha"
        } else {
            "beta"
        };
        fixture.external.memory_entries = vec![beta, alpha];
        let sources = fixture.context();
        let content = bind_memory(&planned_memory(64), &sources);
        assert!(
            content.starts_with(&format!("## User Memories\n{expected_first}")),
            "content_hash tiebreak should be deterministic ascending: {content}"
        );
    }

    fn planned_memory(estimated_tokens: u32) -> PlannedSection {
        PlannedSection {
            kind: SectionKind::Memory,
            scope: crate::section_types::CacheScope::None,
            estimated_tokens,
            priority: crate::section_types::CompressionPriority::Normal,
            source: crate::section_types::SectionSource::Memory,
        }
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
            has_memory: !sources.external.memory_entries.is_empty(),
            model_id: &sources.session.model_id,
            query_source: "repl",
        };
        let plan = plan_turn(&plan_input);
        let bound = bind_all(&plan, &sources);

        assert_eq!(bound.sections.len(), plan.sections.len());
        assert!(!bound.messages.is_empty());
    }
}
