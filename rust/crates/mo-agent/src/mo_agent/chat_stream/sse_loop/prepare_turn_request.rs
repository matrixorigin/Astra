//! Assemble one outbound `/chat` JSON body: base payload, memory boost, tool selection, `edge_tools`, explain stderr, `record_plan`.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use mo_agent_runtime::{
    pipeline::step_recorder::StepRecorder,
    tool_registry::{self, apply_selector_hints_to_edge_profile},
    tool_selector,
    turn::boost_domain_hints::domain_hints_from_boost_terms,
    turn::chat_turn_edge_profile::{
        detect_active_system_skills_in_message, read_git_branch_abbrev,
    },
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_active_skills_into_edge_profile,
        merge_skill_instructions_into_edge_profile, set_payload_edge_tools,
        set_payload_tool_results_if_non_empty,
    },
    turn::tool_schema_prune::{filter_tool_schemas_by_excluded_names, pin_invoked_tool_schemas},
    turn::turn_guard::TurnGuard,
};
use serde_json::Value;

use super::super::edge_executor::edge_executor_instance_id;
use super::explain_sidecar::{eprint_restricted_tools_explain, eprint_selector_guidance_explain};
use super::skill_instructions_round::{load_skill_instructions_text, merge_skill_names_track};

use crate::edge_tools;
use crate::skill_instructions::SharedSkillRegistry;

/// First-turn / cross-turn counters updated while building the payload.
pub(crate) struct PrepareTurnTelemetry<'a> {
    pub first_memoria_ms: &'a mut Option<u64>,
    pub first_selector_ms: &'a mut Option<u64>,
    pub first_selector_strategy: &'a mut Option<String>,
    pub selector_tokens_in: &'a mut u64,
    pub selector_tokens_out: &'a mut u64,
    pub first_selection_report: &'a mut Option<tool_registry::SelectionReport>,
    pub first_budget_pressure: &'a mut f64,
    pub first_context_assembly_ms: &'a mut Option<u64>,
    pub all_selected_skills: &'a mut Vec<String>,
}

pub(crate) struct PrepareChatTurnRequest<'a> {
    pub messages: &'a [Value],
    pub current_session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub explain_verbose: bool,
    pub explain_on: bool,
    pub explain_stderr: bool,
    pub project_root: &'a Path,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub executor: &'a mut edge_tools::ToolExecutor,
    pub selector: &'a dyn tool_selector::ToolSelector,
    pub registry: &'a tool_registry::ToolRegistry,
    pub tool_results: &'a [Value],
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub quiet: bool,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
}

pub(crate) async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> Value {
    let git_branch = read_git_branch_abbrev();
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: ctx.messages,
        session_id: ctx.current_session_id,
        model: ctx.model,
        explain_verbose: ctx.explain_verbose,
        explain_on: ctx.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: mo_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
    });
    let active_skills = detect_active_system_skills_in_message(ctx.message);
    merge_active_skills_into_edge_profile(&mut payload, &active_skills);

    let budget_pressure = {
        let schema_tokens = ctx.selector.registry().total_pinned_token_cost();
        let estimated = mo_agent_runtime::prompts::estimate_tokens_precise(
            ctx.messages,
            schema_tokens as usize,
            0,
        );
        let budget = mo_agent_runtime::prompts::budget_for_model(ctx.model);
        let tier = budget.compaction_tier(estimated);
        tier.budget_pressure()
    };

    let mut boost_terms =
        mo_agent_runtime::turn::retrieval::extract_boost_terms_from_pairs(ctx.history, ctx.message);
    {
        let mem_start = Instant::now();
        let memory_contents = ctx.executor.memory_boost_search(ctx.message, 5).await;
        let mem_elapsed = mem_start.elapsed().as_millis() as u64;
        if ctx.telem.first_memoria_ms.is_none() {
            *ctx.telem.first_memoria_ms = Some(mem_elapsed);
        }
        if !memory_contents.is_empty() {
            for content in &memory_contents {
                for repo in extract_repos_from_memory(content) {
                    ctx.executor.add_preferred_repo(&repo);
                }
            }
            let ranked = mo_agent_runtime::turn::retrieval::rank_memory_results(
                ctx.message,
                &memory_contents,
            );
            mo_agent_runtime::turn::retrieval::append_boost_terms_from_ranked_memory(
                &mut boost_terms,
                ctx.message,
                &ranked,
            );
        }
    }

    let memory_domain_hints = domain_hints_from_boost_terms(&boost_terms);
    for tool in ctx.turn_guard.health.deprioritized_tools() {
        ctx.restricted_tools.insert(tool.to_string());
    }
    let restricted_vec: Vec<String> = ctx.restricted_tools.iter().cloned().collect();

    ctx.step_recorder.record_perceive(
        ctx.message,
        &[],
        &memory_domain_hints
            .iter()
            .map(|h| format!("{h:?}"))
            .collect::<Vec<_>>(),
        &boost_terms,
    );

    let learned_context = ctx.selector.learned_context(ctx.message, ctx.recent_tools);
    let learned_context_hint = learned_context.prompt_fragment();
    let learned_task_type = learned_context
        .task_archetype
        .map(|task_type| format!("{task_type:?}").to_lowercase());

    let mut selected_skills: Vec<String> = Vec::new();
    let (turn_schemas, selection_report, selection_confidence) = if ctx.tool_results.is_empty() {
        let sel_start = Instant::now();
        let turn_count = ctx.history.len() as u32 + 1;
        let sel_ctx = tool_selector::SelectionContext {
            query: ctx.message,
            turn_count,
            recent_tools: ctx.recent_tools,
            budget_tokens: ctx.registry.default_budget(),
            boost_terms: boost_terms.clone(),
            budget_pressure,
            memory_domain_hints: memory_domain_hints.clone(),
            restricted_tools: restricted_vec.clone(),
            file_context: ctx.file_context.to_vec(),
        };
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        if ctx.telem.first_selector_ms.is_none() {
            *ctx.telem.first_selector_ms = Some(sel_start.elapsed().as_millis() as u64);
            *ctx.telem.first_selector_strategy = Some(format!(
                "{} (conf={:.2})",
                sel_result.strategy, sel_result.confidence
            ));
        }
        *ctx.telem.selector_tokens_in += sel_result.selector_tokens_in;
        *ctx.telem.selector_tokens_out += sel_result.selector_tokens_out;
        selected_skills = sel_result.selected_skills.clone();
        let conf = sel_result.confidence;
        let (schemas, report) = tool_selector::resolve_schemas_with_pressure(
            ctx.registry,
            &sel_result.tool_names,
            budget_pressure,
        );
        (schemas, report, conf)
    } else {
        let turn_count = ctx.history.len() as u32 + 1;
        let sel_ctx = tool_selector::SelectionContext {
            query: ctx.message,
            turn_count,
            recent_tools: ctx.recent_tools,
            budget_tokens: ctx.registry.default_budget() * 2,
            boost_terms,
            budget_pressure,
            memory_domain_hints,
            restricted_tools: restricted_vec,
            file_context: ctx.file_context.to_vec(),
        };
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        if !sel_result.selected_skills.is_empty() {
            selected_skills = sel_result.selected_skills.clone();
        }
        let conf = sel_result.confidence;
        let (mut selected, mut report) = tool_selector::resolve_schemas_with_pressure(
            ctx.registry,
            &sel_result.tool_names,
            budget_pressure,
        );
        pin_invoked_tool_schemas(
            &mut selected,
            &mut report,
            ctx.tool_results,
            ctx.all_schemas,
        );
        (selected, report, conf)
    };

    let skill_instructions =
        load_skill_instructions_text(ctx.skill_registry, &selected_skills, ctx.quiet);
    merge_skill_names_track(ctx.telem.all_selected_skills, &selected_skills);

    merge_skill_instructions_into_edge_profile(&mut payload, skill_instructions.as_deref());

    if ctx.telem.first_selection_report.is_none() {
        *ctx.telem.first_selection_report = Some(selection_report);
        *ctx.telem.first_budget_pressure = budget_pressure;
    }
    ctx.executor.set_budget_pressure(budget_pressure);

    apply_selector_hints_to_edge_profile(
        &mut payload["edge_profile"],
        ctx.telem.first_selection_report.as_ref(),
        selection_confidence,
        &learned_context_hint,
        learned_task_type.as_deref(),
    );
    let final_schemas = filter_tool_schemas_by_excluded_names(turn_schemas, ctx.restricted_tools);
    set_payload_edge_tools(&mut payload, final_schemas);
    eprint_restricted_tools_explain(ctx.explain_stderr, ctx.restricted_tools);
    eprint_selector_guidance_explain(ctx.explain_stderr, &payload, selection_confidence);
    set_payload_tool_results_if_non_empty(&mut payload, ctx.tool_results);

    {
        let selected_tool_names: Vec<String> = ctx
            .telem
            .first_selection_report
            .as_ref()
            .map(|r| r.tools_selected.clone())
            .unwrap_or_default();
        let bp = *ctx.telem.first_budget_pressure;
        let bt = ctx
            .telem
            .first_selection_report
            .as_ref()
            .map(|r| r.budget_used as u64)
            .unwrap_or(0);
        ctx.step_recorder
            .record_plan(&selected_tool_names, selection_confidence, bp, bt);
    }

    if ctx.telem.first_context_assembly_ms.is_none() {
        *ctx.telem.first_context_assembly_ms =
            Some(ctx.assembly_start.elapsed().as_millis() as u64);
    }

    payload
}
