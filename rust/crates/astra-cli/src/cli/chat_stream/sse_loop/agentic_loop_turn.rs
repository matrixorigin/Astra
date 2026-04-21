//! Outbound `/chat/turn` payload preparation + fetch + SSE consume.
//!
//! The heavy orchestrator (`run_agentic_loop_iteration`) has been replaced by
//! the runtime's [`run_agentic_loop_with_host`]; this module now only exposes
//! `fetch_chat_turn_sse` for use by [`super::cli_loop_host::CliAgenticLoopHost`].

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use astra_runtime::{
    pipeline::step_recorder::StepRecorder,
    prompts,
    tool_registry::{self, ToolRegistry},
    tool_selector::{self, ToolSelector},
    turn::agentic_loop_host::{TurnInteractionMode, TurnInteractionPolicy},
    turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools,
    turn::agentic_turn_telemetry::{
        accumulate_selector_token_usage, capture_first_selection_report_if_empty,
        record_first_latency_ms_since, record_first_selector_confidence,
        record_first_selector_latency_and_strategy,
    },
    turn::boost_domain_hints::{domain_hints_debug_strings, domain_hints_from_boost_terms},
    turn::chat_turn_api_error::{
        CHAT_TURN_POST_MAX_RETRIES, chat_turn_http_error_with_compact_body,
    },
    turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn,
    turn::chat_turn_edge_profile::{
        detect_active_system_skills_in_message, read_git_branch_abbrev,
    },
    turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_active_skills_into_edge_profile,
        set_payload_tool_results_if_non_empty,
    },
    turn::chat_turn_selection_context::build_agentic_tool_selection_context,
    turn::chat_turn_step_plan::record_agentic_step_plan_after_payload_prep,
    turn::prepare_turn_explain_text::explain_stderr_payload_line_pair,
    turn::tool_schema_prune::pin_invoked_tool_schemas,
    turn::turn_guard::{TurnGuard, merge_deprioritized_tools_into_restricted},
};
use crossterm::style::Stylize;
use serde_json::{Value, json};

use crate::{
    ExplainMode,
    cli_utils::compact_or_raw,
    edge_tools::ToolExecutor,
    permission_manager::PermissionManager,
    stream_render::{
        ChatPrepPhaseLabel, ChatTurnPrepLineGuard, EdgeSseContext, RenderPolicy, TurnResult,
        consume_turn_sse,
    },
};

use super::super::edge_executor::edge_executor_instance_id;

/// Per-phase stderr timings for `/chat/turn`. Enable with `ASTRA_CHAT_TURN_TIMING=1`
/// or `MO_DEBUG=1`.
pub(crate) fn chat_turn_timing_stderr_enabled() -> bool {
    std::env::var("ASTRA_CHAT_TURN_TIMING")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        || std::env::var("MO_DEBUG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn log_chat_turn_timing_phase(timing: bool, label: &str, mark: &mut Instant) {
    if !timing {
        return;
    }
    let ms = mark.elapsed().as_millis();
    eprintln!("{}", format!("  [chat-turn timing] {label}: {ms}ms").dim());
    *mark = Instant::now();
}

/// Updates the live stderr prep line (`Ns  Phase… ⠿`, braille animates at end) for normal chat.
fn touch_prep_ui_phase(phase: &Option<ChatPrepPhaseLabel>, label: &str) {
    if let Some(a) = phase
        && let Ok(mut w) = a.write()
    {
        *w = label.to_string();
    }
}

fn msg_content(m: &Value) -> String {
    match m.get("content") {
        Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
        Some(v) if v.is_array() => v
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn message_has_tool_calls(m: &Value) -> bool {
    m.get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

fn semantic_query_from_message(message: &str) -> Cow<'_, str> {
    let trimmed = message.trim();
    if !trimmed.starts_with("[Active task attachment]") {
        return Cow::Borrowed(message);
    }

    let mut latest_task = None;
    let mut assistant_summary = Vec::new();
    let mut followup = Vec::new();
    let mut in_summary = false;
    let mut in_followup = false;

    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = line.strip_prefix("Latest user task: ") {
            latest_task = Some(rest.to_string());
            in_summary = false;
            in_followup = false;
            continue;
        }
        if line == "Latest assistant summary:" {
            in_summary = true;
            in_followup = false;
            continue;
        }
        if line == "[User follow-up]" {
            in_summary = false;
            in_followup = true;
            continue;
        }
        if line.starts_with("Recent tools: ") || line.starts_with("Artifact: ") {
            in_summary = false;
            in_followup = false;
            continue;
        }
        if in_summary && assistant_summary.len() < 3 {
            assistant_summary.push(line.to_string());
        } else if in_followup {
            followup.push(line.to_string());
        }
    }

    let mut parts = Vec::new();
    if let Some(task) = latest_task {
        parts.push(format!("Task: {task}"));
    }
    if !assistant_summary.is_empty() {
        parts.push(format!(
            "Assistant summary: {}",
            assistant_summary.join(" ")
        ));
    }
    if !followup.is_empty() {
        parts.push(format!("Follow-up: {}", followup.join(" ")));
    }

    if parts.is_empty() {
        Cow::Borrowed(message)
    } else {
        Cow::Owned(parts.join("\n"))
    }
}

fn should_skip_memory_boost(message: &str, history: &[(String, String)]) -> bool {
    !history.is_empty() && matches!(semantic_query_from_message(message), Cow::Owned(_))
}

fn retained_history_messages(messages: &[Value]) -> &[Value] {
    match messages.split_last() {
        Some((last, history)) if last.get("role").and_then(Value::as_str) == Some("user") => {
            history
        }
        _ => messages,
    }
}

fn build_retained_history_turns(
    messages: &[Value],
) -> Vec<astra_runtime::turn::context_assembly_trace::TurnRetention> {
    let mut turns = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let tokens = prompts::estimate_str_tokens(&msg_content(message)) as u32;
        let has_tool_calls = message_has_tool_calls(message);

        if turns.is_empty() || role == "user" {
            turns.push(astra_runtime::turn::context_assembly_trace::TurnRetention {
                turn_index: turns.len() as u32,
                role,
                tokens,
                has_tool_calls,
            });
            continue;
        }

        if let Some(turn) = turns.last_mut() {
            turn.tokens += tokens;
            turn.has_tool_calls |= has_tool_calls;
            if retained_turn_role_priority(&role) > retained_turn_role_priority(&turn.role) {
                turn.role = role;
            }
        }
    }

    turns
}

fn retained_turn_role_priority(role: &str) -> u8 {
    match role {
        "assistant" => 3,
        "user" => 2,
        "system" => 1,
        _ => 0,
    }
}

// ─── Outbound `/chat` JSON body (was `prepare_turn_request.rs`) ───────────────

/// First-turn / cross-turn counters updated while building the payload.
pub(crate) struct PrepareTurnTelemetry<'a> {
    pub first_memoria_ms: &'a mut Option<u64>,
    pub first_selector_ms: &'a mut Option<u64>,
    pub first_selector_strategy: &'a mut Option<String>,
    pub first_selector_confidence: &'a mut Option<f64>,
    pub selector_tokens_in: &'a mut u64,
    pub selector_tokens_out: &'a mut u64,
    pub first_selection_report: &'a mut Option<tool_registry::SelectionReport>,
    pub first_budget_pressure: &'a mut f64,
    pub first_context_assembly_ms: &'a mut Option<u64>,
    pub all_selected_skills: &'a mut Vec<String>,
    /// Optional trace collector for observability (M1).
    pub trace_collector: Option<&'a astra_runtime::turn::turn_trace_collector::TurnTraceCollector>,
}

struct PrepareChatTurnRequest<'a> {
    messages: &'a [Value],
    ephemeral_prefix: Option<&'a Value>,
    current_session_id: Option<&'a str>,
    model: Option<&'a str>,
    explain: AgenticChatExplainFlags,
    project_root: &'a Path,
    message: &'a str,
    history: &'a [(String, String)],
    recent_tools: &'a [String],
    executor: &'a mut ToolExecutor,
    selector: &'a dyn tool_selector::ToolSelector,
    registry: &'a tool_registry::ToolRegistry,
    tool_results: &'a [Value],
    all_schemas: &'a [Value],
    turn_guard: &'a TurnGuard,
    restricted_tools: &'a mut HashSet<String>,
    step_recorder: &'a mut StepRecorder,
    file_context: &'a [String],
    assembly_start: Instant,
    telem: PrepareTurnTelemetry<'a>,
    skill_search: &'a astra_core::SkillSearchSettings,
    is_plan_subtask: bool,
    plan_subtask_id: Option<&'a str>,
    /// When true, emit `[chat-turn timing] …` lines to stderr (see `chat_turn_timing_stderr_enabled`).
    timing_phases: bool,
    /// Normal chat: human-readable step shown after the elapsed second count on stderr.
    prep_ui_phase: Option<ChatPrepPhaseLabel>,
    /// Effort level override from skill activation.
    skill_effort: Option<String>,
    /// Agent type hint from skill activation.
    skill_agent_type: Option<String>,
    /// Scenario-driven override for the tool selection token budget.
    tool_budget_override: Option<u32>,
    interaction_mode: TurnInteractionMode,
    turn_policy: &'a mut TurnInteractionPolicy,
    previous_confidence_fallback:
        Option<astra_runtime::turn::confidence_contract::ConfidenceFallback>,
    /// Current agentic loop round (0-based). Sent to bridge for round budget directives.
    round_index: u32,
}

pub(crate) fn turn_policy_from_payload_edge_tools(
    payload: &Value,
    interaction_mode: TurnInteractionMode,
) -> TurnInteractionPolicy {
    payload
        .get("edge_tools")
        .and_then(Value::as_array)
        .map(|tools| TurnInteractionPolicy::from_tool_schemas(interaction_mode, tools))
        .unwrap_or_else(|| {
            TurnInteractionPolicy::from_visible_tool_names(interaction_mode, Vec::new())
        })
}

async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> Value {
    let timing = ctx.timing_phases;
    let mut mark = Instant::now();
    let prep_wall = mark;

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Starting…");

    let git_branch = read_git_branch_abbrev();
    let thinking_budget_tokens: Option<u32> = std::env::var("MO_THINKING_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: ctx.messages,
        session_id: ctx.current_session_id,
        agent_id: Some("astra-cli"),
        model: ctx.model,
        explain_verbose: ctx.explain.explain_verbose,
        explain_on: ctx.explain.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: astra_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
        thinking_budget_tokens,
    });

    // Inject ephemeral prefix (e.g., skill listing) at the start of messages.
    if let Some(prefix) = ctx.ephemeral_prefix {
        if let Some(arr) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            arr.insert(0, prefix.clone());
        }
    }
    let active_skills = detect_active_system_skills_in_message(ctx.message);
    merge_active_skills_into_edge_profile(&mut payload, &active_skills);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Reading workspace…");
    let passive_msgs = ctx
        .executor
        .take_passive_workspace_diagnostic_messages(ctx.project_root, !ctx.tool_results.is_empty())
        .await;
    if !passive_msgs.is_empty()
        && let Some(root) = payload.as_object_mut()
        && let Some(messages) = root.get_mut("messages")
        && let Some(arr) = messages.as_array_mut()
    {
        for m in passive_msgs {
            arr.push(m);
        }
    }
    log_chat_turn_timing_phase(timing, "base_payload_passive_workspace", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Recalling memory…");

    let budget_pressure = {
        let schema_tokens = ctx.selector.registry().total_pinned_token_cost();
        budget_pressure_for_chat_turn(ctx.messages, ctx.model, schema_tokens as usize)
    };

    let semantic_query = semantic_query_from_message(ctx.message);
    let semantic_query_str = semantic_query.as_ref();
    let mut boost_terms = astra_runtime::turn::retrieval::extract_boost_terms_from_pairs(
        ctx.history,
        semantic_query_str,
    );
    let mut memoria_insights_text: Option<String> = None;
    {
        if should_skip_memory_boost(ctx.message, ctx.history) {
            if let Some(collector) = ctx.telem.trace_collector {
                collector.record_memory_retrieval(semantic_query_str, 0, &[], 0);
            }
        } else {
            let mem_start = Instant::now();
            let memory_hits = ctx
                .executor
                .memory_boost_search(semantic_query_str, 5)
                .await;
            let mem_latency_ms = mem_start.elapsed().as_millis() as u64;
            record_first_latency_ms_since(ctx.telem.first_memoria_ms, mem_start);

            // Always record memory retrieval trace, even when no hits (for observability)
            let memory_contents: Vec<String> =
                memory_hits.iter().map(|h| h.content.clone()).collect();
            let ranked = if memory_contents.is_empty() {
                Vec::new()
            } else {
                astra_runtime::turn::retrieval::rank_memory_results(
                    semantic_query_str,
                    &memory_contents,
                )
            };
            if let Some(collector) = ctx.telem.trace_collector {
                collector.record_memory_retrieval(
                    semantic_query_str,
                    memory_contents.len() as u32,
                    &ranked,
                    mem_latency_ms,
                );
            }

            if !memory_hits.is_empty() {
                for content in &memory_contents {
                    for repo in extract_repos_from_memory(content) {
                        ctx.executor.add_preferred_repo(&repo);
                    }
                }
                astra_runtime::turn::retrieval::append_boost_terms_from_ranked_memory(
                    &mut boost_terms,
                    semantic_query_str,
                    &ranked,
                );
                memoria_insights_text =
                    astra_runtime::memoria_insights::render_digest(&memory_contents);
                // Send "useful" feedback for retrieved memories (fire-and-forget)
                let feedback_ids: Vec<String> = memory_hits
                    .iter()
                    .filter_map(|h| h.memory_id.clone())
                    .collect();
                ctx.executor.memory_feedback_useful(feedback_ids);
            }
        }
    }
    log_chat_turn_timing_phase(timing, "memory_boost_search", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Preparing tools…");

    let memory_domain_hints = domain_hints_from_boost_terms(&boost_terms);
    merge_deprioritized_tools_into_restricted(ctx.turn_guard, ctx.restricted_tools);
    let restricted_vec: Vec<String> = ctx.restricted_tools.iter().cloned().collect();

    ctx.step_recorder.record_perceive(
        semantic_query_str,
        &[],
        &domain_hints_debug_strings(&memory_domain_hints),
        &boost_terms,
    );

    let learned_context = ctx
        .selector
        .learned_context(semantic_query_str, ctx.recent_tools);
    let learned_context_hint = learned_context.prompt_fragment();
    let learned_task_type = learned_context.task_archetype_payload_token();

    // Skill activation is handled exclusively by the `skill` tool in the agentic loop
    // (see turn/skill_tool.rs + partition_and_execute_skills). The model decides when
    // to invoke skills by calling the tool, rather than having skills pre-injected by
    // the selector.

    let (
        turn_schemas,
        selection_report,
        selection_confidence,
        selection_strategy,
        _selection_tokens_in,
        _selection_tokens_out,
        selection_latency_ms,
    ) = if ctx.tool_results.is_empty() {
        let sel_start = Instant::now();
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Scanning context…");
        let sel_ctx = build_agentic_tool_selection_context(
            semantic_query_str,
            ctx.history.len(),
            ctx.recent_tools,
            ctx.registry,
            boost_terms.clone(),
            budget_pressure,
            memory_domain_hints.clone(),
            restricted_vec.clone(),
            ctx.file_context.to_vec(),
            false,
            ctx.tool_budget_override,
            ctx.previous_confidence_fallback.clone(),
        );
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Thinking…");
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        let sel_latency_ms = sel_start.elapsed().as_millis() as u64;
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
        record_first_selector_latency_and_strategy(
            ctx.telem.first_selector_ms,
            ctx.telem.first_selector_strategy,
            sel_start,
            sel_result.strategy,
            sel_result.confidence,
        );
        record_first_selector_confidence(
            ctx.telem.first_selector_confidence,
            sel_result.confidence,
        );
        accumulate_selector_token_usage(
            ctx.telem.selector_tokens_in,
            ctx.telem.selector_tokens_out,
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
        );

        let conf = sel_result.confidence;
        let (schemas, report) = tool_selector::resolve_schemas_with_pressure(
            ctx.registry,
            &sel_result.tool_names,
            budget_pressure,
        );
        (
            schemas,
            report,
            conf,
            sel_result.strategy.to_string(),
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
            sel_latency_ms,
        )
    } else {
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Continuing…");
        let sel_ctx = build_agentic_tool_selection_context(
            semantic_query_str,
            ctx.history.len(),
            ctx.recent_tools,
            ctx.registry,
            boost_terms,
            budget_pressure,
            memory_domain_hints,
            restricted_vec,
            ctx.file_context.to_vec(),
            true,
            ctx.tool_budget_override,
            ctx.previous_confidence_fallback.clone(),
        );
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Thinking…");
        let sel_start = Instant::now();
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        let sel_latency_ms = sel_start.elapsed().as_millis() as u64;
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
        accumulate_selector_token_usage(
            ctx.telem.selector_tokens_in,
            ctx.telem.selector_tokens_out,
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
        );
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
        (
            selected,
            report,
            conf,
            sel_result.strategy.to_string(),
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
            sel_latency_ms,
        )
    };
    log_chat_turn_timing_phase(timing, "tool_selector_resolve_schemas", &mut mark);

    let selected_tool_costs: Vec<(String, u32)> = selection_report
        .tools_selected
        .iter()
        .map(|name| (name.clone(), ctx.registry.token_cost(name)))
        .collect();
    let selected_tool_tokens_total: u32 = selected_tool_costs.iter().map(|(_, cost)| *cost).sum();

    if let Some(collector) = ctx.telem.trace_collector {
        collector.record_tool_selection(
            &selection_report.tools_selected,
            &selection_strategy,
            selection_confidence,
            &selected_tool_costs,
            ctx.registry.total_tool_count() as u32,
            selection_latency_ms,
        );
    }

    capture_first_selection_report_if_empty(
        ctx.telem.first_selection_report,
        ctx.telem.first_budget_pressure,
        selection_report.clone(),
        budget_pressure,
    );
    ctx.executor.set_budget_pressure(budget_pressure);

    apply_selector_hints_then_attach_filtered_edge_tools(
        &mut payload,
        turn_schemas,
        ctx.restricted_tools,
        ctx.telem.first_selection_report.as_ref(),
        selection_confidence,
        learned_context_hint.as_str(),
        learned_task_type.as_deref(),
    );
    *ctx.turn_policy = turn_policy_from_payload_edge_tools(&payload, ctx.interaction_mode);
    log_chat_turn_timing_phase(timing, "skill_merge_attach_edge_tools", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Finishing up…");

    if ctx.explain.explain_stderr {
        let (restricted_line, guidance_line) =
            explain_stderr_payload_line_pair(ctx.restricted_tools, &payload, selection_confidence);
        match (&restricted_line, &guidance_line) {
            (Some(r), Some(g)) => eprintln!("{}", format!("{r}  ·  {g}").dim()),
            (Some(r), None) => eprintln!("{}", r.as_str().dim()),
            (None, Some(g)) => eprintln!("{}", g.as_str().dim()),
            (None, None) => {}
        }
    }
    set_payload_tool_results_if_non_empty(&mut payload, ctx.tool_results);

    record_agentic_step_plan_after_payload_prep(
        ctx.step_recorder,
        ctx.telem.first_selection_report.as_ref(),
        *ctx.telem.first_budget_pressure,
        selection_confidence,
    );

    record_first_latency_ms_since(ctx.telem.first_context_assembly_ms, ctx.assembly_start);

    inject_runtime_turn_overrides(
        &mut payload,
        ctx.skill_search,
        ctx.is_plan_subtask,
        ctx.plan_subtask_id,
        ctx.skill_effort.as_deref(),
        ctx.skill_agent_type.as_deref(),
    );

    // Inject round_index so the bridge can add round budget directives.
    if let Some(root) = payload.as_object_mut() {
        root.insert("round_index".into(), json!(ctx.round_index));
    }

    // ─── SelfModel: inject self-awareness text into edge_profile ───
    if let Some(self_model) = ctx.executor.build_self_model_snapshot() {
        let text = self_model.to_system_prompt_section();
        if text.len() > 30 {
            if let Some(root) = payload.as_object_mut()
                && let Some(ep) = root.get_mut("edge_profile")
                && let Some(ep_obj) = ep.as_object_mut()
            {
                ep_obj.insert("self_awareness_text".to_string(), json!(text));
            }
        }
    }
    // ─── Memoria insights: inject recall digest into edge_profile ───
    if let Some(ref insights) = memoria_insights_text
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("memoria_insights_text".to_string(), json!(insights));
    }
    log_chat_turn_timing_phase(timing, "self_awareness_inject", &mut mark);

    // ─── Record token budget estimate to trace collector (M1 observability) ───
    if let Some(collector) = ctx.telem.trace_collector {
        let schema_tokens = selected_tool_tokens_total;
        let budget = prompts::budget_for_model(ctx.model);
        let max_tokens = budget.model_limit as u32;
        let history_messages = retained_history_messages(ctx.messages);

        // Estimate retained history tokens from prior messages only.
        let history_tokens: u32 = history_messages
            .iter()
            .map(|m| prompts::estimate_str_tokens(&msg_content(m)) as u32)
            .sum();

        // Record per-turn history breakdown
        let turns_retained = build_retained_history_turns(history_messages);
        collector.set_history_retained(&turns_retained);

        // Estimate user message tokens
        let user_message_tokens = prompts::estimate_str_tokens(ctx.message) as u32;

        // System prompt tokens: the system prompt is assembled by the runtime
        // (bridge_inprocess.rs) and sent back via `context_meta` SSE event.
        // Use 0 here as placeholder — runtime will overwrite via record_token_budget.
        let system_prompt_tokens = 0u32;

        // Memory tokens are tracked in memory retrieval trace, use 0 here
        // (would need to be passed from memory boost search results)
        let memory_tokens = 0u32;

        let estimated_total = system_prompt_tokens
            + history_tokens
            + memory_tokens
            + schema_tokens
            + user_message_tokens;

        collector.record_token_budget_estimate(
            system_prompt_tokens,
            history_tokens,
            memory_tokens,
            schema_tokens,
            user_message_tokens,
            estimated_total,
            max_tokens,
            budget_pressure,
        );
    }

    log_chat_turn_timing_phase(timing, "finalize_payload_records", &mut mark);
    if timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] prepare_payload_wall_total: {}ms",
                prep_wall.elapsed().as_millis()
            )
            .dim()
        );
    }

    payload
}

fn inject_runtime_turn_overrides(
    payload: &mut Value,
    skill_search: &astra_core::SkillSearchSettings,
    is_plan_subtask: bool,
    plan_subtask_id: Option<&str>,
    skill_effort: Option<&str>,
    skill_agent_type: Option<&str>,
) {
    let Some(root) = payload.as_object_mut() else {
        return;
    };

    root.insert(
        "skill_search".into(),
        serde_json::to_value(skill_search).unwrap_or_else(|_| json!({})),
    );

    if is_plan_subtask {
        root.insert("is_plan_subtask".into(), json!(true));
        root.insert("rollback_on_failure".into(), json!(true));
        root.insert("rollback_boundary".into(), json!("turn"));
    }
    if let Some(id) = plan_subtask_id.map(str::trim).filter(|s| !s.is_empty()) {
        root.insert("plan_subtask_id".into(), json!(id));
    }
    if let Some(effort) = skill_effort {
        root.insert("effort".into(), json!(effort));
    }
    if let Some(agent_type) = skill_agent_type {
        root.insert("agent_type".into(), json!(agent_type));
    }
}

// `load_skill_instructions_text` removed — skill activation now goes through
// the `skill` tool in the agentic loop, not through proactive payload injection.

// ─── Fetch: payload → POST → consume_turn_sse ─────────────────────────────────

pub(crate) struct ChatTurnSseFetchRequest<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: &'a Path,
    pub executor: &'a mut ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: &'a ToolRegistry,
    pub messages: &'a [Value],
    /// Ephemeral system message prepended to messages for this turn only
    /// (e.g., skill listing). Not stored in conversation history.
    pub ephemeral_prefix: Option<&'a Value>,
    pub current_session_id: Option<&'a str>,
    pub tool_results: &'a [Value],
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a astra_runtime::turn::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
    pub perm_manager: &'a mut PermissionManager,
    pub skill_search: &'a astra_core::SkillSearchSettings,
    /// Lines from the previous headless tool round that must be cleared
    /// before the next SSE stream starts rendering.
    pub pre_clear_lines: usize,
    pub is_plan_subtask: bool,
    pub plan_subtask_id: Option<&'a str>,
    /// Optional cancellation token for interrupting SSE streaming.
    pub cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Plan-only: release the payload-phase stderr line before SSE consumes the body.
    pub plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::super::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<super::super::ApprovalRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls.
    pub skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Effort level override from skill activation.
    pub skill_effort: Option<String>,
    /// Agent type hint from skill activation.
    pub skill_agent_type: Option<String>,
    /// Scenario-driven override for the tool selection token budget.
    pub tool_budget_override: Option<u32>,
    pub interaction_mode: TurnInteractionMode,
    pub turn_policy: &'a mut TurnInteractionPolicy,
    /// When true, this is a continuation turn after a skill has already produced output.
    /// Propagated to `EdgeSseContext` to buffer text and suppress thinking previews.
    pub skill_continuation: bool,
    /// Cross-turn tool output cache (persists across turns via `CliAgenticLoopHost`).
    pub tool_cache: &'a mut crate::stream_render::EdgeToolCache,
    /// Fallback from previous turn's confidence diagnosis for broadening.
    pub previous_confidence_fallback:
        Option<astra_runtime::turn::confidence_contract::ConfidenceFallback>,
    /// Current agentic loop round (0-based). Sent to bridge for round budget directives.
    pub round_index: u32,
}
struct ChatTurnSseFetchUi {
    timing: bool,
    show_prep_line: bool,
    prep_ui_phase: Option<ChatPrepPhaseLabel>,
}

fn chat_turn_sse_fetch_ui(
    render_policy: RenderPolicy,
    plan_assemble_line_release: Option<&Arc<AtomicBool>>,
) -> ChatTurnSseFetchUi {
    let timing = chat_turn_timing_stderr_enabled();
    if timing {
        eprintln!(
            "{}",
            "  [chat-turn timing] starting prepare_chat_turn_payload…".dim()
        );
    }

    // Normal chat: one stderr status line during payload + HTTP (plan mode uses the outer
    // `PlanAssembleLineSpinner` + `plan_assemble_line_release` instead). Disabled with timing
    // stderr lines to avoid `\r` / `eprintln!` fighting.
    let show_prep_line = !render_policy.suppress_text()
        && std::io::stderr().is_terminal()
        && !timing
        && plan_assemble_line_release.is_none();

    let prep_ui_phase = if show_prep_line {
        Some(Arc::new(std::sync::RwLock::new("Starting…".to_string())))
    } else {
        None
    };

    ChatTurnSseFetchUi {
        timing,
        show_prep_line,
        prep_ui_phase,
    }
}

/// Build JSON payload (with optional prep line), POST `/chat/turn`, return response + prep guard.
///
/// The caller must drop [`ChatTurnPrepLineGuard`] when entering SSE consume (`consume_turn_sse`)
/// or on early error after reading the body, so the stderr status line stays through TTFB.
async fn chat_turn_post_payload_after_prepare(
    api: &astra_thin_client::ThinClient,
    token: &str,
    quiet: bool,
    ui: &ChatTurnSseFetchUi,
    prepare: PrepareChatTurnRequest<'_>,
) -> Result<(astra_thin_client::HttpResponse, ChatTurnPrepLineGuard), String> {
    let prep_line = ChatTurnPrepLineGuard::maybe_start(ui.show_prep_line, ui.prep_ui_phase.clone());
    let payload = prepare_chat_turn_payload(prepare).await;

    touch_prep_ui_phase(&ui.prep_ui_phase, "Sending…");
    let http_mark = Instant::now();
    let resp = api
        .post_chat_turn_retry_429(token, &payload, CHAT_TURN_POST_MAX_RETRIES, quiet)
        .await
        .map_err(|e| e.to_string())?;
    if ui.timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] http_post_until_response_object: {}ms",
                http_mark.elapsed().as_millis()
            )
            .dim()
        );
    }
    Ok((resp, prep_line))
}

pub(crate) async fn fetch_chat_turn_sse(
    ctx: ChatTurnSseFetchRequest<'_>,
) -> Result<TurnResult, String> {
    let ChatTurnSseFetchRequest {
        api,
        token,
        model,
        explain,
        render_md,
        term_width,
        render_policy,
        message,
        history,
        recent_tools,
        project_root,
        executor,
        selector,
        registry,
        messages,
        ephemeral_prefix,
        current_session_id,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        file_context,
        assembly_start,
        telem,
        perm_manager,
        skill_search,
        pre_clear_lines,
        is_plan_subtask,
        plan_subtask_id,
        cancel_token,
        plan_assemble_line_release,
        stream_event_tx,
        approval_request_tx,
        skill_resolver,
        skill_effort,
        skill_agent_type,
        tool_budget_override,
        interaction_mode,
        turn_policy,
        skill_continuation,
        tool_cache,
        previous_confidence_fallback,
        round_index,
    } = ctx;

    let ui = chat_turn_sse_fetch_ui(render_policy, plan_assemble_line_release.as_ref());

    let (resp, prep_line) = chat_turn_post_payload_after_prepare(
        api,
        token,
        render_policy.is_silent(),
        &ui,
        PrepareChatTurnRequest {
            messages,
            ephemeral_prefix,
            current_session_id,
            model,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(match explain {
                ExplainMode::Off => AgenticExplainUiMode::Off,
                ExplainMode::On => AgenticExplainUiMode::On,
                ExplainMode::Verbose => AgenticExplainUiMode::Verbose,
            }),
            project_root,
            message,
            history,
            recent_tools,
            executor,
            selector,
            registry,
            tool_results,
            all_schemas,
            turn_guard,
            restricted_tools,
            step_recorder,
            file_context,
            assembly_start,
            telem,
            skill_search,
            is_plan_subtask,
            plan_subtask_id,
            timing_phases: ui.timing,
            prep_ui_phase: ui.prep_ui_phase.clone(),
            skill_effort,
            skill_agent_type,
            tool_budget_override,
            interaction_mode,
            turn_policy,
            previous_confidence_fallback,
            round_index,
        },
    )
    .await?;

    let status = resp.status();
    if !status.is_success() {
        drop(prep_line);
        let body = resp.text().await.map_err(|e| e.to_string())?;
        return Err(chat_turn_http_error_with_compact_body(
            status.as_u16(),
            body.as_str(),
            compact_or_raw,
        ));
    }

    // Paired with `Acquire` load in `PlanAssembleLineSpinner`: publish "HTTP response ready"
    // so the spinner thread always sees POST completion before SSE consumes the body (UI only).
    if let Some(flag) = plan_assemble_line_release.as_ref() {
        flag.store(true, Ordering::Release);
    }

    let edge_ctx = EdgeSseContext {
        api,
        token,
        executor_id: edge_executor_instance_id(),
        executor,
        render_policy,
        perm_manager: Some(perm_manager),
        cancel_token,
        stream_event_tx,
        approval_request_tx,
        skill_resolver,
        skill_continuation,
        turn_rollback_on_failure: is_plan_subtask,
        tool_cache,
    };

    let sse_mark = Instant::now();
    let turn = consume_turn_sse(
        prep_line,
        resp,
        render_md,
        term_width,
        render_policy,
        Some(edge_ctx),
        pre_clear_lines,
        cancel_token,
    )
    .await;
    if ui.timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] sse_consume_total: {}ms  ttft_ms: {:?}",
                sse_mark.elapsed().as_millis(),
                turn.ttft_ms
            )
            .dim()
        );
    }

    Ok(turn)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use astra_runtime::turn::agentic_loop_host::{ASK_USER_TOOL_NAME, TurnInteractionMode};
    use astra_runtime::turn::chat_history_openai::merge_skill_names_track;
    use serde_json::json;

    fn schema(name: &str) -> serde_json::Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("{name} tool"),
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into()]);
        assert_eq!(v, vec!["a", "b"]);
    }

    #[test]
    fn inject_runtime_turn_overrides_adds_skill_search_and_plan_fields() {
        let mut payload = json!({});
        super::inject_runtime_turn_overrides(
            &mut payload,
            &astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            },
            true,
            Some("sub-1"),
            Some("high"),
            Some("coder"),
        );

        assert_eq!(payload["skill_search"]["dynamic_surface"], json!(false));
        assert_eq!(payload["skill_search"]["min_catalog_size"], json!(12));
        assert_eq!(payload["skill_search"]["surface_cap"], json!(20));
        assert_eq!(payload["is_plan_subtask"], json!(true));
        assert_eq!(payload["rollback_on_failure"], json!(true));
        assert_eq!(payload["rollback_boundary"], json!("turn"));
        assert_eq!(payload["plan_subtask_id"], json!("sub-1"));
        assert_eq!(payload["effort"], json!("high"));
        assert_eq!(payload["agent_type"], json!("coder"));
    }
    #[test]
    fn msg_content_extracts_string_and_array_formats() {
        // String content (OpenAI format)
        let str_msg = json!({"role": "user", "content": "hello world"});
        assert!(!super::msg_content(&str_msg).is_empty());

        // Array content (Anthropic format)
        let arr_msg = json!({"role": "user", "content": [
            {"type": "text", "text": "hello "},
            {"type": "text", "text": "world"}
        ]});
        assert_eq!(super::msg_content(&arr_msg), "hello world");

        // Null/missing content
        let null_msg = json!({"role": "assistant", "content": null});
        assert!(super::msg_content(&null_msg).is_empty());
    }

    #[test]
    fn retained_history_excludes_current_user_and_groups_messages_by_turn() {
        let messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "reply one"}),
            json!({"role": "user", "content": "second"}),
            json!({"role": "assistant", "content": "calling tool", "tool_calls": [{"id": "call-1"}]}),
            json!({"role": "tool", "content": "tool output"}),
            json!({"role": "assistant", "content": "final answer"}),
            json!({"role": "user", "content": "current"}),
        ];

        let history_messages = super::retained_history_messages(&messages);
        assert_eq!(history_messages.len(), 6);

        let turns = super::build_retained_history_turns(history_messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_index, 0);
        assert_eq!(turns[0].role, "assistant");
        assert!(!turns[0].has_tool_calls);
        assert_eq!(turns[1].turn_index, 1);
        assert_eq!(turns[1].role, "assistant");
        assert!(turns[1].has_tool_calls);
    }

    #[test]
    fn retained_history_trailing_system_message_does_not_override_assistant_turn() {
        let messages = vec![
            json!({"role": "user", "content": "review latest commit"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call-1"}]}),
            json!({"role": "tool", "content": "# Skill: review-changes\n..."}),
            json!({"role": "system", "content": "## Already Fetched (do NOT re-read/re-grep these)\nshell.rs"}),
        ];

        let turns = super::build_retained_history_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "assistant");
        assert!(turns[0].has_tool_calls);
    }

    #[test]
    fn retained_history_keeps_system_role_for_system_only_history() {
        let messages = vec![json!({"role": "system", "content": "system note"})];

        let turns = super::build_retained_history_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "system");
        assert!(!turns[0].has_tool_calls);
    }

    #[test]
    fn semantic_query_from_attachment_compacts_wrapper_text() {
        let message = "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
Latest user task: review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b\n\
Latest assistant summary:\n\
## Review: `aa1f419b` — P5 git timeout, P6 compression protection\n\
Two independent fixes in one commit. Let me review each.\n\
P5 still has a thread leak on timeout; terminate the child before returning.\n\n\
[User follow-up]\n修复?";
        let semantic = super::semantic_query_from_message(message);
        let semantic = semantic.as_ref();

        assert!(semantic.contains("Task: review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b"));
        assert!(semantic.contains("Assistant summary: ## Review: `aa1f419b`"));
        assert!(semantic.contains("Follow-up: 修复?"));
        assert!(!semantic.contains("[Active task attachment]"));
        assert!(!semantic.contains("Treat brief follow-ups"));
    }

    #[test]
    fn plain_message_keeps_borrowed_semantic_query() {
        let semantic = super::semantic_query_from_message("fix the timeout path");
        assert!(matches!(semantic, std::borrow::Cow::Borrowed(_)));
        assert_eq!(semantic.as_ref(), "fix the timeout path");
    }

    #[test]
    fn active_task_attachment_skips_memory_boost_once_history_exists() {
        let history = vec![(
            "review 这个: aa1f419b".to_string(),
            "Need to fix timeout.".to_string(),
        )];
        let attachment = "[Active task attachment]\nLatest user task: review 这个: aa1f419b\nLatest assistant summary:\nNeed to fix timeout.\n\n[User follow-up]\n修复?";
        assert!(super::should_skip_memory_boost(attachment, &history));
        assert!(!super::should_skip_memory_boost(attachment, &[]));
        assert!(!super::should_skip_memory_boost(
            "fix the timeout path",
            &history
        ));
    }

    #[test]
    fn context_meta_sse_event_sets_system_prompt_tokens() {
        use astra_runtime::turn::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };
        let mut accum = ChatTurnSseAccum::default();
        let sse = "data: {\"type\":\"context_meta\",\"system_prompt_tokens\":5432}\n\n";
        dispatch_chat_turn_sse_event_block(sse, &mut accum, &mut vec![]);
        assert_eq!(accum.system_prompt_tokens, Some(5432));
    }

    #[test]
    fn context_meta_sse_event_parses_full_breakdown() {
        use astra_runtime::turn::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };
        use astra_runtime::turn::context_assembly_trace::SystemPromptBreakdown;

        let mut accum = ChatTurnSseAccum::default();
        let sse = concat!(
            "data: {\"type\":\"context_meta\",\"system_prompt_tokens\":8000,",
            "\"system_prompt_breakdown\":{",
            "\"base_persona_tokens\":6000,\"environment_tokens\":500,",
            "\"user_preferences_tokens\":200,",
            "\"skills_injected\":[{\"skill_name\":\"concise\",\"tokens\":100,",
            "\"selection_reason\":\"active_output_skill\"}],",
            "\"repository_memories\":[{\"memory_id\":\"prefetch-0\",",
            "\"memory_type\":\"hybrid_retrieval\",\"tokens\":80,",
            "\"relevance_score\":0.9,\"content_preview\":\"user likes rust\"}],",
            "\"total_tokens\":8000}}\n\n"
        );
        dispatch_chat_turn_sse_event_block(sse, &mut accum, &mut vec![]);
        assert_eq!(accum.system_prompt_tokens, Some(8000));

        let bd: SystemPromptBreakdown =
            serde_json::from_value(accum.system_prompt_breakdown.unwrap()).unwrap();
        assert_eq!(bd.base_persona_tokens, 6000);
        assert_eq!(bd.environment_tokens, 500);
        assert_eq!(bd.user_preferences_tokens, 200);
        assert_eq!(bd.skills_injected.len(), 1);
        assert_eq!(bd.skills_injected[0].skill_name, "concise");
        assert_eq!(bd.skills_injected[0].tokens, 100);
        assert_eq!(bd.repository_memories.len(), 1);
        assert_eq!(bd.repository_memories[0].memory_id, "prefetch-0");
        assert_eq!(bd.repository_memories[0].tokens, 80);
        assert_eq!(bd.total_tokens, 8000);
    }

    #[test]
    fn turn_policy_from_payload_edge_tools_tracks_final_payload_tools() {
        let payload = json!({
            "edge_tools": [schema("mo_query"), schema(ASK_USER_TOOL_NAME)]
        });

        let policy =
            super::turn_policy_from_payload_edge_tools(&payload, TurnInteractionMode::Prompt);

        assert_eq!(
            policy.visible_tool_names,
            vec!["mo_query".to_string(), ASK_USER_TOOL_NAME.to_string()]
        );
        assert_eq!(policy.evidence_tool_names, vec!["mo_query".to_string()]);
        assert!(policy.allow_ask_user);
    }

    #[test]
    fn turn_policy_from_payload_edge_tools_defaults_empty_when_missing() {
        let policy =
            super::turn_policy_from_payload_edge_tools(&json!({}), TurnInteractionMode::Auto);

        assert!(policy.visible_tool_names.is_empty());
        assert!(policy.evidence_tool_names.is_empty());
        assert!(!policy.allow_ask_user);
    }
}

// Note: Environment variable parsing tests for `chat_turn_timing_stderr_enabled` were removed
// because unsafe `std::env::set_var` is unsound in multi-threaded programs. The function's
// logic is trivial (pattern matching on env var values) and not worth testing with unsafe code.
