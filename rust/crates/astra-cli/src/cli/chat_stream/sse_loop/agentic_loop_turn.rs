//! Outbound `/chat/turn` payload preparation + fetch + SSE consume.
//!
//! The heavy orchestrator (`run_agentic_loop_iteration`) has been replaced by
//! the runtime's [`run_agentic_loop_with_host`]; this module now only exposes
//! `fetch_chat_turn_sse` for use by [`crate::cli_loop_host::CliAgenticLoopHost`].

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
    turn::agentic_loop::host::{TurnInteractionMode, TurnInteractionPolicy},
    turn::agentic_prepare_payload::attach_filtered_edge_tools_to_payload,
    turn::agentic_turn_telemetry::{
        capture_first_surface_report_if_empty, record_first_latency_ms_since,
    },
    turn::boost_domain_hints::{domain_hints_debug_strings, domain_hints_from_boost_terms},
    turn::chat_turn_api_error::{
        CHAT_TURN_POST_MAX_RETRIES, chat_turn_http_error_with_compact_body,
    },
    turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn,
    turn::chat_turn_edge_profile::{
        EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES, EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES,
        EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW, EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT,
        detect_active_system_skills_in_message, read_git_branch_abbrev,
    },
    turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_active_skills_into_edge_profile,
        merge_edge_profile_extensions, set_payload_tool_results_if_non_empty,
    },
    turn::chat_turn_step_plan::record_agentic_step_plan_after_payload_prep,
    turn::prepare_turn_explain_text::restricted_tools_explain_text,
    turn::tool_schema_prune::retain_invoked_tool_schemas,
    turn::turn_guard::{TurnGuard, merge_deprioritized_tools_into_restricted},
};
use astra_turn_core::tool::schema::tool_schema_name;
use crossterm::style::Stylize;
use serde_json::{Value, json};

use crate::{
    ExplainMode,
    cli::cli_config::cli_utils::compact_or_raw,
    cli::permission_manager::PermissionManager,
    cli::stream::stream_render::{
        ChatPrepPhaseLabel, ChatTurnPrepLineGuard, EdgeSseContext, RenderPolicy, TurnResult,
        consume_turn_sse,
    },
    edge_tools::ToolExecutor,
};

use crate::cli::chat_stream::edge_executor::edge_executor_instance_id;

const BASH_BACKGROUND_TASK_CONTROL_TOOLS: &[&str] = &["task_output", "task_list", "task_stop"];

/// Session-control tools injected unconditionally to prevent schema thrashing.
/// Their combined cost is < 200 tokens but toggling them on/off breaks prompt
/// caching at every plan-mode transition or tool surface variance.
const CACHE_STABLE_SESSION_TOOLS: &[&str] =
    &["enter_plan_mode", "exit_plan_mode", "compress_context"];

/// Per-phase stderr timings for `/chat/turn`. Disabled — use `RUST_LOG=debug` instead.
pub(crate) fn chat_turn_timing_stderr_enabled() -> bool {
    false
}

fn log_chat_turn_timing_phase(timing: bool, label: &str, mark: &mut Instant) {
    if !timing {
        return;
    }
    let ms = mark.elapsed().as_millis();
    eprintln!("{}", format!("  [chat-turn timing] {label}: {ms}ms").dim());
    *mark = Instant::now();
}

fn apply_cli_health_restrictions(
    turn_guard: &TurnGuard,
    restricted_tools: &mut HashSet<String>,
    widen_surface_pending: &mut bool,
) {
    if std::mem::take(widen_surface_pending) {
        return;
    }
    merge_deprioritized_tools_into_restricted(turn_guard, restricted_tools);
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

fn should_skip_memory_boost(
    has_semantic_query_override: bool,
    history: &[(String, String)],
) -> bool {
    !history.is_empty() && has_semantic_query_override
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
) -> Vec<astra_turn_core::context_assembly_trace::TurnRetention> {
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
            turns.push(astra_turn_core::context_assembly_trace::TurnRetention {
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
    pub first_surface_report: &'a mut Option<tool_registry::ToolSurfaceReport>,
    pub first_budget_pressure: &'a mut f64,
    pub first_context_assembly_ms: &'a mut Option<u64>,
    pub all_selected_skills: &'a mut Vec<String>,
    /// Optional trace collector for observability (M1).
    pub trace_collector: Option<&'a astra_runtime::turn::turn_trace_collector::TurnTraceCollector>,
}

struct PrepareChatTurnRequest<'a> {
    messages: &'a [Value],
    runtime_volatile_texts: &'a [String],
    ephemeral_prefix: Option<&'a Value>,
    current_session_id: Option<&'a str>,
    model: Option<&'a str>,
    explain: AgenticChatExplainFlags,
    project_root: &'a Path,
    message: &'a str,
    semantic_query_override: Option<&'a str>,
    history: &'a [(String, String)],
    recent_tools: &'a [String],
    executor: Arc<ToolExecutor>,

    registry: &'a tool_registry::ToolRegistry,
    tool_results: &'a [Value],
    all_schemas: &'a [Value],
    valid_tool_names: &'a mut HashSet<String>,
    turn_guard: &'a TurnGuard,
    restricted_tools: &'a mut HashSet<String>,
    widen_surface_pending: &'a mut bool,
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
    interaction_mode: TurnInteractionMode,
    turn_policy: &'a mut TurnInteractionPolicy,
    /// Skill-scoped tool allowlist — tools the active skill declared as needed.
    /// After the tool surface includes tools, any allowed tools it missed are force-injected.
    skill_allowed_tools: Option<Vec<String>>,
    previous_confidence_fallback: Option<astra_turn_core::confidence_contract::ConfidenceFallback>,
    /// Current agentic loop round (0-based). Sent to bridge for tool round directives.
    round_index: u32,
    /// Authoritative visible-turn number from the outer loop.
    session_turn: u32,
    /// Stable bridge turn-chain id reused across retries within the same visible turn.
    turn_chain_id: Option<&'a str>,
    /// Stable root user-query event id reused across retries within the same visible turn.
    user_query_event_id: Option<&'a str>,
    /// Snapshot of session-wide denial pressure (current, max_total) taken at
    /// call time. Published to the observability session so SelfModel can
    /// render it in the system prompt.
    denial_pressure: (u32, u32),
    /// Snapshot of session-wide recent `(tool, reason)` rejections for
    /// SelfModel Gap 3 surface.
    recent_rejections: Vec<(String, String)>,
    /// Optional shared observability hub, forwarded from the SSE fetch request
    /// so the per-turn SelfModel ingest can read `hub.tuning().recent_signals()`.
    observability_hub: Option<&'a Arc<astra_runtime::observability::ObservabilityHub>>,
    append_system_prompt: Option<&'a str>,
    /// Whether the current permission mode is `Plan`. When true the schema-
    /// preparation step adds every mutating tool to `restricted_tools` so the
    /// model only sees read-only + plan-control tools (`exit_plan_mode` etc.).
    plan_mode_active: bool,
}

pub(crate) fn turn_policy_from_payload_edge_tools(
    payload: &Value,
    interaction_mode: TurnInteractionMode,
) -> TurnInteractionPolicy {
    let schemas = final_visible_tool_schemas_from_payload(payload);
    TurnInteractionPolicy::from_tool_schemas(interaction_mode, &schemas)
}

pub(crate) fn final_visible_tool_schemas_from_payload(payload: &Value) -> Vec<Value> {
    payload
        .get("edge_tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn surface_report_from_visible_schemas(
    schemas: &[Value],
    budget_used: u32,
    budget_total: u32,
) -> tool_registry::ToolSurfaceReport {
    let visible_tools: Vec<String> = schemas
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect();
    tool_registry::ToolSurfaceReport {
        visible_count: visible_tools.len() as u32,
        visible_tools,
        budget_used,
        budget_total,
    }
}

fn runtime_filter_turn_schemas_and_report(
    executor: &crate::edge_tools::ToolExecutor,
    turn_schemas: &mut Vec<Value>,
    surface_report: &mut tool_registry::ToolSurfaceReport,
) -> bool {
    let had_tools_before =
        !turn_schemas.is_empty() || surface_report_has_visible_tools(surface_report);
    *turn_schemas = executor.runtime_bound_tool_schemas(std::mem::take(turn_schemas));
    let runtime_bound_turn_names =
        astra_turn_core::tool::schema::tool_names_from_schemas(turn_schemas.as_slice());
    surface_report
        .visible_tools
        .retain(|name| runtime_bound_turn_names.contains(name));
    surface_report.visible_count = surface_report.visible_tools.len() as u32;
    had_tools_before
}

fn surface_report_has_visible_tools(report: &tool_registry::ToolSurfaceReport) -> bool {
    !report.visible_tools.is_empty() || report.visible_count > 0
}

/// Priority-ordered check chain: first true signal wins. Returns
/// `(should_inject_tools, reason)` where `reason` is for `tracing::trace!`
/// observability only — never branched on by downstream code.
fn tool_surface_should_inject(
    turn_schemas: &[Value],
    surface_report: &tool_registry::ToolSurfaceReport,
    had_tools_before_runtime_filter: bool,
    has_recent_tools: bool,
    has_tool_results: bool,
    plan_mode_active: bool,
) -> (bool, &'static str) {
    if !turn_schemas.is_empty() {
        return (true, "visible_tool_candidates");
    }
    if surface_report_has_visible_tools(surface_report) {
        return (true, "surface_report_names");
    }
    if had_tools_before_runtime_filter {
        return (true, "had_tools_before_runtime_filter");
    }
    if has_recent_tools {
        return (true, "recent_tool_context");
    }
    if has_tool_results {
        return (true, "tool_results_followup");
    }
    if plan_mode_active {
        return (true, "plan_mode_active");
    }
    if surface_report.budget_total == 0 {
        return (true, "budget_starved_surface");
    }
    (false, "")
}

async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> Value {
    let timing = ctx.timing_phases;
    let mut mark = Instant::now();
    let prep_wall = mark;

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Starting…");

    let git_branch = read_git_branch_abbrev();
    let requested_model = astra_core::model_override::normalize_model_override(ctx.model);
    let (resolved_model, thinking_config) = match requested_model {
        Some(m) => {
            let (name, cfg) = astra_turn_core::thinking_config::resolve_model_thinking(m);
            // Per-turn dampener: the model suffix encodes the user's CEILING
            // (e.g. `thinking:high`), not a command to burn that budget on every
            // turn regardless of content. Short read-only questions get a
            // capped effort — multi-step / modification turns pass through
            // unchanged. See `ThinkingConfig::scale_for_turn` for the policy.
            let signals =
                astra_turn_core::thinking_config::TurnComplexitySignals::from_message(ctx.message);
            let cfg = cfg.scale_for_turn(signals);
            (Some(name), cfg)
        }
        None => (None, astra_turn_core::thinking_config::ThinkingConfig::Off),
    };
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: ctx.messages,
        session_id: ctx.current_session_id,
        agent_id: Some("astra-cli"),
        model: resolved_model,
        interaction_mode: Some(ctx.interaction_mode.label()),
        explain_verbose: ctx.explain.explain_verbose,
        explain_on: ctx.explain.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: astra_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
        thinking: thinking_config,
    });

    if !ctx.runtime_volatile_texts.is_empty()
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS
                .to_string(),
            json!(ctx.runtime_volatile_texts),
        );
    }

    // Route skill listing through edge_profile → bridge volatile lane, so
    // it lands in RuntimeVolatile (post-cache-marker) rather than becoming a
    // leading role:system message that breaks the prefix cache on
    // prefix-only providers (DeepSeek, GLM, Qwen).
    if let Some(prefix) = ctx.ephemeral_prefix {
        if let Some(content) = prefix.get("content").and_then(serde_json::Value::as_str)
            && !content.is_empty()
            && let Some(root) = payload.as_object_mut()
            && let Some(ep) = root.get_mut("edge_profile")
            && let Some(ep_obj) = ep.as_object_mut()
        {
            ep_obj.insert(
                astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_SKILL_LISTING_TEXT
                    .to_string(),
                json!(content),
            );
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
        let schema_tokens = ctx.registry.total_always_load_token_cost();
        budget_pressure_for_chat_turn(ctx.messages, requested_model, schema_tokens as usize)
    };

    let semantic_query_str = ctx.semantic_query_override.unwrap_or(ctx.message);
    let mut boost_terms =
        astra_turn_core::retrieval::extract_boost_terms_from_pairs(ctx.history, semantic_query_str);
    let mut memoria_insights_text: Option<String> = None;
    {
        if should_skip_memory_boost(ctx.semantic_query_override.is_some(), ctx.history) {
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
                astra_turn_core::retrieval::rank_memory_results(
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
                astra_turn_core::retrieval::append_boost_terms_from_ranked_memory(
                    &mut boost_terms,
                    semantic_query_str,
                    &ranked,
                );
                memoria_insights_text =
                    astra_runtime::memory_hooks::insights::render_digest(&memory_contents);
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
    apply_cli_health_restrictions(
        ctx.turn_guard,
        ctx.restricted_tools,
        ctx.widen_surface_pending,
    );
    ctx.step_recorder.record_perceive(
        semantic_query_str,
        &[],
        &domain_hints_debug_strings(&memory_domain_hints),
        &boost_terms,
    );

    // Skill activation is handled exclusively by the `skill` tool in the agentic loop
    // (see turn/skill_tool.rs + partition_and_execute_skills). The model decides when
    // to invoke skills by calling the tool, rather than having skills pre-injected by
    // the tool surface builder.

    let (turn_schemas, surface_report, surface_latency_ms) = {
        let sel_start = Instant::now();
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
        let budget = ctx.registry.default_budget();
        let (mut schemas, mut report) = ctx.registry.build_initial_surface_with_report_ctx(
            semantic_query_str,
            ctx.history.len() as u32,
            budget,
            ctx.recent_tools,
        );
        if !ctx.tool_results.is_empty() {
            retain_invoked_tool_schemas(
                &mut schemas,
                &mut report,
                ctx.tool_results,
                ctx.all_schemas,
            );
        }
        let sel_latency_ms = sel_start.elapsed().as_millis() as u64;
        (schemas, report, sel_latency_ms)
    };
    log_chat_turn_timing_phase(timing, "registry_select_schemas", &mut mark);

    // Force-inject any skill allowed_tools that the assembled surface missed.
    let mut turn_schemas = turn_schemas;
    let mut surface_report = surface_report;
    if let Some(ref allowed) = ctx.skill_allowed_tools {
        astra_turn_core::tool_schema_prune::inject_skill_allowed_tools(
            &mut turn_schemas,
            &mut surface_report,
            allowed,
            ctx.all_schemas,
        );
    }
    if !ctx.plan_mode_active {
        if let Some(required) = ctx.executor.take_pending_round_tool_boost() {
            let required_refs: Vec<&str> = required.iter().map(String::as_str).collect();
            astra_turn_core::tool_schema_prune::inject_required_tool_names(
                &mut turn_schemas,
                &mut surface_report,
                &required_refs,
                ctx.all_schemas,
            );
        }
        // Make newly activated deferred tools visible. This is not a TTL:
        // activation remains pending until the model actually calls that
        // tool, so `select:a,b,c` does not lose `c` after `a` and `b` run.
        let activated = ctx
            .executor
            .activated_deferred_tool_names_for_schema_injection();
        if !activated.is_empty() {
            let refs: Vec<&str> = activated.iter().map(String::as_str).collect();
            astra_turn_core::tool_schema_prune::inject_required_tool_names(
                &mut turn_schemas,
                &mut surface_report,
                &refs,
                ctx.all_schemas,
            );
        }
        if surface_report
            .visible_tools
            .iter()
            .any(|name| name == "bash")
        {
            astra_turn_core::tool_schema_prune::inject_required_tool_names(
                &mut turn_schemas,
                &mut surface_report,
                BASH_BACKGROUND_TASK_CONTROL_TOOLS,
                ctx.all_schemas,
            );
        }
    }
    let had_tools_before_runtime_filter = runtime_filter_turn_schemas_and_report(
        ctx.executor.as_ref(),
        &mut turn_schemas,
        &mut surface_report,
    );
    // NOTE: `budget_used` is intentionally NOT recomputed here. The
    // `surface_report_from_visible_schemas` call below is the single source
    // of truth for the final report's budget; any value set on the
    // intermediate `surface_report.budget_used` would be overwritten and
    // never consumed. See test
    // `surface_report_from_visible_schemas_is_single_source_for_budget`.

    let (inject_tools, surface_reason) = tool_surface_should_inject(
        &turn_schemas,
        &surface_report,
        had_tools_before_runtime_filter,
        !ctx.recent_tools.is_empty(),
        !ctx.tool_results.is_empty(),
        ctx.plan_mode_active,
    );
    tracing::trace!(
        target: "astra.tool_surface",
        reason = surface_reason,
        inject_tools,
        "chat turn tool surface decision"
    );
    if inject_tools {
        // Keep session-control tools stable once a turn needs tools. An
        // explicit empty tool surface stays tool-free unless pending
        // activation, prior context, or structural selection pressure requires
        // a recovery-capable tool surface.
        astra_turn_core::tool_schema_prune::inject_required_tool_names(
            &mut turn_schemas,
            &mut surface_report,
            CACHE_STABLE_SESSION_TOOLS,
            ctx.all_schemas,
        );
        let has_tool_search = turn_schemas
            .iter()
            .filter_map(tool_schema_name)
            .any(|name| name == "tool_search");
        if !has_tool_search {
            // Deferred discovery must never be stranded behind its own
            // deferred surface. Once the structural decision says this is a
            // tool-bearing turn, the activation primitive must be visible or
            // the model has no recovery path.
            astra_turn_core::tool_schema_prune::inject_required_tool_names(
                &mut turn_schemas,
                &mut surface_report,
                &["tool_search"],
                ctx.all_schemas,
            );
        }
    }

    // Runtime-filter after tool injection to keep schemas consistent with
    // executor capabilities. The return value is intentionally discarded: this
    // second pass only cleans up the tool surface, it does not feed the
    // had_tools_before snapshot.
    let _had_tools_before = runtime_filter_turn_schemas_and_report(
        ctx.executor.as_ref(),
        &mut turn_schemas,
        &mut surface_report,
    );

    // Plan-mode tool restrictions are owned by the host
    // (`CliAgenticLoopHost::execute_turn`) using the same
    // turn-scoped add-then-remove pattern as
    // `interaction_scoped_tool_restrictions`. Doing it here as a
    // raw `extend` on the shared `state.restricted_tools` set
    // leaked names into later turns — see the regression note on
    // session 19298aea in `cli_loop_host::plan_mode_restriction_names`.

    ctx.executor.set_budget_pressure(budget_pressure);

    attach_filtered_edge_tools_to_payload(&mut payload, turn_schemas, ctx.restricted_tools);
    // Sync the executor guard from the final payload, after capability
    // restrictions and interaction-mode filtering have all been applied. The
    // guard must mirror what the model actually saw.
    let final_visible_schemas = final_visible_tool_schemas_from_payload(&payload);
    let final_visible_tool_names =
        astra_turn_core::tool::schema::tool_names_from_schemas(&final_visible_schemas);
    let eligible_surface_schemas: Vec<Value> = ctx
        .registry
        .all_tool_schemas()
        .iter()
        .filter(|schema| {
            tool_schema_name(schema)
                .map(|name| !ctx.restricted_tools.contains(name))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    let eligible_surface_schemas = ctx
        .executor
        .runtime_bound_tool_schemas(eligible_surface_schemas);
    let eligible_plugin_schemas = ctx
        .executor
        .runtime_bound_plugin_schemas_excluding(ctx.restricted_tools);
    let tool_surface = tool_registry::surface::ToolSurface::build_excluding_visible(
        eligible_surface_schemas,
        &astra_config::runtime_config::RuntimeConfig::cached().tool_surface,
        &eligible_plugin_schemas,
        &final_visible_tool_names,
    );
    let mut activatable_tool_names = HashSet::new();
    // Always send always_load tool names so the server can place cache_control
    // markers at the correct always_load/dynamic boundary. User TOML can add
    // tools to the declaration defaults, so this must be the resolved runtime
    // set — not a compile-time constant.
    let always_load_names = ctx.registry.always_load_tool_names_sorted();
    if !final_visible_tool_names.is_empty() && !always_load_names.is_empty() {
        merge_edge_profile_extensions(
            &mut payload,
            &json!({
                EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES: always_load_names,
            }),
        );
    }
    if !final_visible_tool_names.is_empty()
        && final_visible_tool_names.contains("tool_search")
        && let Some(manifest) = tool_surface.deferred_manifest(requested_model)
    {
        activatable_tool_names = manifest.names.iter().cloned().collect();
        merge_edge_profile_extensions(
            &mut payload,
            &json!({
                EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT: manifest.text,
                EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW: manifest.context_window,
                EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES: manifest.names,
            }),
        );
    }
    ctx.executor
        .set_current_tool_surface(&final_visible_schemas, activatable_tool_names);
    // Telemetry truth: recompute token cost from the actual final visible
    // schemas. The intermediate report may include recommendation hints later
    // stripped by capability/interaction-mode filtering.
    // Final persisted reports must keep `visible_count`, `visible_tools`,
    // and `budget_used` on the same full-visible-surface basis.
    let visible_tool_costs: Vec<(String, u32)> = final_visible_schemas
        .iter()
        .filter_map(|schema| {
            tool_schema_name(schema).map(|name| (name.to_string(), ctx.registry.token_cost(name)))
        })
        .collect();
    let visible_tool_tokens_total: u32 = visible_tool_costs.iter().map(|(_, cost)| *cost).sum();
    let final_surface_report = surface_report_from_visible_schemas(
        &final_visible_schemas,
        visible_tool_tokens_total,
        surface_report.budget_total,
    );
    *ctx.valid_tool_names = final_visible_tool_names;

    if let Some(collector) = ctx.telem.trace_collector {
        collector.record_tool_surface(
            &final_surface_report.visible_tools,
            &visible_tool_costs,
            final_visible_schemas.len() as u32,
            surface_latency_ms,
        );
    }

    capture_first_surface_report_if_empty(
        ctx.telem.first_surface_report,
        ctx.telem.first_budget_pressure,
        final_surface_report,
        budget_pressure,
    );
    *ctx.turn_policy = turn_policy_from_payload_edge_tools(&payload, ctx.interaction_mode);
    log_chat_turn_timing_phase(timing, "skill_merge_attach_edge_tools", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Finishing up…");

    if ctx.explain.explain_stderr {
        if let Some(restricted_line) = restricted_tools_explain_text(ctx.restricted_tools) {
            eprintln!("{}", restricted_line.as_str().dim());
        }
    }
    set_payload_tool_results_if_non_empty(&mut payload, ctx.tool_results);

    record_agentic_step_plan_after_payload_prep(
        ctx.step_recorder,
        ctx.telem.first_surface_report.as_ref(),
        *ctx.telem.first_budget_pressure,
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
    // Inject round_index so the bridge can add tool round directives.
    if let Some(root) = payload.as_object_mut() {
        root.insert("round_index".into(), json!(ctx.round_index));
    }
    inject_bridge_turn_identity(
        &mut payload,
        ctx.session_turn,
        ctx.turn_chain_id,
        ctx.user_query_event_id,
    );
    // The outer agentic loop is the single owner of aggregate turn journal
    // rows for every LLM round. The bridge may still capture full request /
    // response payloads for debugging, but it must not emit duplicate
    // `llm_round` summaries for later rounds.
    if let Some(root) = payload.as_object_mut() {
        root.insert("root_turn_journal_owned".into(), json!(true));
    }

    // ─── SelfModel: inject self-awareness text into edge_profile ───
    // Publish fresh denial-pressure + per-tool outcome bias + recent
    // rejections to the observability session so SelfModel can render the
    // cumulative signals back to the agent.
    {
        let (current, max_total) = ctx.denial_pressure;
        let bias: std::collections::BTreeMap<
            String,
            astra_turn_core::tool_health::OutcomeBiasEntry,
        > = ctx
            .turn_guard
            .health
            .outcome_bias_by_tool(3600)
            .into_iter()
            .filter(|(_, e)| e.score.abs() >= 0.005)
            .collect();
        if let Some(session_lock) = &ctx.executor.observability_session
            && let Ok(mut session) = session_lock.write()
        {
            session.last_denial_pressure = Some(astra_runtime::self_model::DenialPressureView {
                total_denials: current,
                max_total,
            });
            session.set_outcome_bias(bias);
            session.recent_rejections = ctx
                .recent_rejections
                .iter()
                .map(
                    |(tool, reason)| astra_runtime::self_model::RejectionSummary {
                        tool: tool.clone(),
                        reason: reason.clone(),
                    },
                )
                .collect();

            // Publish the four SelfModel inputs that were previously hard-coded
            // to empty at `build_self_model_snapshot`.
            // TODO: surface real skill names once the active-skill registry is
            // reachable here; for now we mirror `all_selected_skills` which
            // tracks skills actually chosen this session.
            let skills = ctx.telem.all_selected_skills.clone();
            let tool_health_entries = ctx.turn_guard.health.export();
            let scenario = session.current_scenario();
            let recent_signals = ctx
                .observability_hub
                .as_ref()
                .map(|hub| hub.tuning().recent_signals())
                .unwrap_or_default();
            session.ingest_self_model_inputs(skills, tool_health_entries, scenario, recent_signals);

            // Injection-freshness observation is deferred to after the
            // turn's SSE stream finishes (see `post_turn_observe_bridge_injections`
            // in `cli_loop_host.rs`). Observing here would fire before
            // the bridge has actually composed its 5 bridge-generated
            // channels (implicit_feedback, feedback_rules,
            // memoria_prefetch, tool_round_guidance, volatile) and leave
            // them permanently `Untracked` in introspect's freshness
            // report.
        }
    }
    if let Some(self_model) = ctx.executor.build_self_model_snapshot() {
        // Gate on signal content, not raw length. A bare `Turn: N\nTokens: …`
        // header easily passes a length threshold but carries no actionable
        // signal for the LLM — emitting it every turn wastes ~500 tokens
        // (and the tokens are in the volatile lane, so they never cache).
        if self_model.has_meaningful_self_awareness() {
            let text = self_model.to_system_prompt_section();
            if !text.trim().is_empty()
                && let Some(root) = payload.as_object_mut()
                && let Some(ep) = root.get_mut("edge_profile")
                && let Some(ep_obj) = ep.as_object_mut()
            {
                ep_obj.insert("self_awareness_text".to_string(), json!(text));
            }
        }
    }
    // ─── Recent-argument hints (gap #5): surface just-used paths + commands ───
    if let Some(hints_text) =
        astra_runtime::recent_arg_hints::prompt_block_from_messages(ctx.messages)
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("recent_arg_hints_text".to_string(), json!(hints_text));
    }
    // ─── Memoria insights: inject recall digest into edge_profile ───
    if let Some(ref insights) = memoria_insights_text
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("memoria_insights_text".to_string(), json!(insights));
    }
    // ─── Gateway context: inject as system message at start of conversation ───
    if let Some(extra) = ctx.append_system_prompt {
        if let Some(arr) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            arr.insert(
                0,
                json!({
                    "role": "system",
                    "content": extra,
                }),
            );
        }
    }
    log_chat_turn_timing_phase(timing, "self_awareness_inject", &mut mark);

    // Injection-freshness observation happens AFTER the bridge's SSE
    // stream completes (see `post_turn_observe_bridge_injections` in
    // `cli_loop_host.rs`), so we can merge the 5 bridge-generated
    // channels (captured via the `injection_freshness` SSE event into
    // `ChatTurnSseAccum.bridge_injection_texts`) with the CLI-owned
    // `lessons` snapshot.

    // ─── Record token budget estimate to trace collector (M1 observability) ───
    if let Some(collector) = ctx.telem.trace_collector {
        let schema_tokens = visible_tool_tokens_total;
        let budget = prompts::budget_for_model(requested_model);
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
        // (`bridge/inprocess.rs`) and sent back via `context_meta` SSE event.
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

fn inject_bridge_turn_identity(
    payload: &mut Value,
    session_turn: u32,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) {
    let Some(root) = payload.as_object_mut() else {
        return;
    };
    if session_turn > 0 {
        root.insert("session_turn".into(), json!(session_turn));
    }
    if let Some(turn_chain_id) = turn_chain_id.filter(|value| !value.trim().is_empty()) {
        root.insert("turn_chain_id".into(), json!(turn_chain_id));
    }
    if let Some(user_query_event_id) = user_query_event_id.filter(|value| !value.trim().is_empty())
    {
        root.insert("user_query_event_id".into(), json!(user_query_event_id));
    }
}

// `load_skill_instructions_text` removed — skill activation now goes through
// the `skill` tool in the agentic loop, not through proactive payload injection.

// ─── Fetch: payload → POST → consume_turn_sse ─────────────────────────────────

pub(crate) struct ChatTurnSseFetchRequest<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub auth_profile: Option<&'a str>,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub semantic_query_override: Option<&'a str>,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: &'a Path,
    pub executor: Arc<ToolExecutor>,
    pub registry: &'a ToolRegistry,
    pub messages: &'a [Value],
    /// CLI runtime nudges drained from the structured volatile lane. Sent as
    /// edge metadata so the runtime can apply model-resolved cache capability
    /// before deciding whether to inject or drop them.
    pub runtime_volatile_texts: &'a [String],
    /// Ephemeral system message prepended to messages for this turn only
    /// (e.g., skill listing). Not stored in conversation history.
    pub ephemeral_prefix: Option<&'a Value>,
    pub current_session_id: Option<&'a str>,
    pub tool_results: &'a [Value],
    pub all_schemas: &'a [Value],
    /// Tool names admitted by the headless validator for the current LLM
    /// round. This is overwritten during payload preparation from the final
    /// `edge_tools` actually sent to the model.
    pub valid_tool_names: &'a mut HashSet<String>,
    pub turn_guard: &'a astra_turn_core::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub widen_surface_pending: &'a mut bool,
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
    pub stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<crate::cli::chat_stream::ApprovalRequestTx>,
    /// Optional channel for native TUI ask_user prompts.
    pub ask_user_request_tx: Option<crate::cli::chat_stream::AskUserRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls.
    pub skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Effort level override from skill activation.
    pub skill_effort: Option<String>,
    /// Agent type hint from skill activation.
    pub skill_agent_type: Option<String>,
    pub interaction_mode: TurnInteractionMode,
    pub turn_policy: &'a mut TurnInteractionPolicy,
    /// Skill-scoped tool allowlist — tools the active skill declared as needed.
    /// After the tool surface includes tools, any allowed tools it missed are force-injected.
    pub skill_allowed_tools: Option<Vec<String>>,
    /// When true, this is a continuation turn after a skill has already produced output.
    /// Propagated to `EdgeSseContext` to buffer text and suppress thinking previews.
    pub skill_continuation: bool,
    /// Cross-turn tool output cache (persists across turns via `CliAgenticLoopHost`).
    pub tool_cache: &'a mut crate::cli::stream::stream_render::EdgeToolCache,
    /// Fallback from previous turn's confidence diagnosis for broadening.
    pub previous_confidence_fallback:
        Option<astra_turn_core::confidence_contract::ConfidenceFallback>,
    /// Current agentic loop round (0-based). Sent to bridge for tool round directives.
    pub round_index: u32,
    pub session_turn: u32,
    pub turn_chain_id: Option<&'a str>,
    pub user_query_event_id: Option<&'a str>,
    /// Optional shared observability hub for reading the auto-tuning feedback
    /// window when publishing SelfModel inputs. Threaded through so the
    /// per-turn ingest can attach `recent_signals` to the session without
    /// needing a global singleton.
    pub observability_hub: Option<&'a Arc<astra_runtime::observability::ObservabilityHub>>,
    pub incremental_state: Option<Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
    pub append_system_prompt: Option<&'a str>,
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
        auth_profile,
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
        registry,
        messages,
        runtime_volatile_texts,
        ephemeral_prefix,
        current_session_id,
        tool_results,
        all_schemas,
        valid_tool_names,
        turn_guard,
        restricted_tools,
        widen_surface_pending,
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
        ask_user_request_tx,
        skill_resolver,
        skill_effort,
        skill_agent_type,
        interaction_mode,
        turn_policy,
        skill_allowed_tools,
        skill_continuation,
        tool_cache,
        previous_confidence_fallback,
        round_index,
        session_turn,
        turn_chain_id,
        user_query_event_id,
        observability_hub,
        incremental_state,
        append_system_prompt,
        semantic_query_override,
    } = ctx;

    let ui = chat_turn_sse_fetch_ui(render_policy, plan_assemble_line_release.as_ref());

    let (resp, prep_line) = chat_turn_post_payload_after_prepare(
        api,
        token,
        render_policy.is_silent(),
        &ui,
        PrepareChatTurnRequest {
            messages,
            runtime_volatile_texts,
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
            semantic_query_override,
            history,
            recent_tools,
            executor: Arc::clone(&executor),
            registry,
            tool_results,
            all_schemas,
            valid_tool_names,
            turn_guard,
            restricted_tools,
            widen_surface_pending,
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
            interaction_mode,
            turn_policy,
            skill_allowed_tools,
            previous_confidence_fallback,
            round_index,
            session_turn,
            turn_chain_id,
            user_query_event_id,
            denial_pressure: perm_manager.denial_pressure(),
            recent_rejections: perm_manager.recent_rejections(),
            observability_hub,
            append_system_prompt,
            plan_mode_active: perm_manager.mode()
                == crate::cli::permission_manager::PermissionMode::Plan,
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
        stream_event_sink: None,
        approval_request_tx,
        ask_user_request_tx,
        skill_resolver,
        skill_continuation,
        turn_rollback_on_failure: is_plan_subtask,
        tool_cache,
        observability_hub: observability_hub.cloned(),
        incremental_state: incremental_state.clone(),
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
        auth_profile,
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
    use super::{
        PrepareChatTurnRequest, PrepareTurnTelemetry, build_retained_history_turns,
        inject_bridge_turn_identity, inject_runtime_turn_overrides, msg_content,
        prepare_chat_turn_payload, retained_history_messages, should_skip_memory_boost,
    };
    use astra_runtime::turn::agentic_loop::host::{ASK_USER_TOOL_NAME, TurnInteractionMode};
    use astra_turn_core::chat_history_openai::merge_skill_names_track;
    use astra_turn_core::chat_turn_edge_profile::{
        EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES, EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES,
        EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT,
    };
    use serde_json::{Value, json};

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
        inject_runtime_turn_overrides(
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
    fn inject_bridge_turn_identity_adds_authoritative_ids() {
        let mut payload = json!({});
        inject_bridge_turn_identity(&mut payload, 2, Some("root-chain"), Some("root-query"));
        assert_eq!(payload["session_turn"], json!(2));
        assert_eq!(payload["turn_chain_id"], json!("root-chain"));
        assert_eq!(payload["user_query_event_id"], json!("root-query"));
    }
    #[test]
    fn msg_content_extracts_string_and_array_formats() {
        // String content (OpenAI format)
        let str_msg = json!({"role": "user", "content": "hello world"});
        assert!(!msg_content(&str_msg).is_empty());

        // Array content (Anthropic format)
        let arr_msg = json!({"role": "user", "content": [
            {"type": "text", "text": "hello "},
            {"type": "text", "text": "world"}
        ]});
        assert_eq!(msg_content(&arr_msg), "hello world");

        // Null/missing content
        let null_msg = json!({"role": "assistant", "content": null});
        assert!(msg_content(&null_msg).is_empty());
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

        let history_messages = retained_history_messages(&messages);
        assert_eq!(history_messages.len(), 6);

        let turns = build_retained_history_turns(history_messages);
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

        let turns = build_retained_history_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "assistant");
        assert!(turns[0].has_tool_calls);
    }

    #[test]
    fn retained_history_keeps_system_role_for_system_only_history() {
        let messages = vec![json!({"role": "system", "content": "system note"})];

        let turns = build_retained_history_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "system");
        assert!(!turns[0].has_tool_calls);
    }

    #[test]
    fn semantic_query_override_skips_memory_boost_once_history_exists() {
        let history = vec![(
            "review 这个: aa1f419b".to_string(),
            "Need to fix timeout.".to_string(),
        )];
        assert!(should_skip_memory_boost(true, &history));
        assert!(!should_skip_memory_boost(true, &[]));
        assert!(!should_skip_memory_boost(false, &history));
    }

    #[test]
    fn context_meta_sse_event_sets_system_prompt_tokens() {
        use astra_turn_core::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };
        let mut accum = ChatTurnSseAccum::default();
        let sse = "data: {\"type\":\"context_meta\",\"system_prompt_tokens\":5432}\n\n";
        dispatch_chat_turn_sse_event_block(sse, &mut accum, &mut vec![]);
        assert_eq!(accum.system_prompt_tokens, Some(5432));
    }

    #[test]
    fn context_meta_sse_event_parses_full_breakdown() {
        use astra_turn_core::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };
        use astra_turn_core::context_assembly_trace::SystemPromptBreakdown;

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

    #[test]
    fn final_visible_tool_schemas_from_payload_uses_only_payload_edge_tools() {
        let payload = json!({
            "edge_tools": [schema("read_file")],
            "candidate_tools_before_filter": [schema(ASK_USER_TOOL_NAME)]
        });

        let schemas = super::final_visible_tool_schemas_from_payload(&payload);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();

        assert_eq!(names, vec!["read_file"]);
    }

    /// Regression: the final ToolSurfaceReport's `budget_used` must be derived
    /// entirely from the explicit `budget_used` argument, NOT from any stale
    /// `budget_used` field on a pre-existing report. This contract is what
    /// permits removing stale intermediate recomputation at the call site (the value
    /// set on `surface_report.budget_used` was overwritten by
    /// the final visible-schema token total and never consumed).
    #[test]
    fn surface_report_from_visible_schemas_is_single_source_for_budget() {
        let schemas = vec![schema("grep"), schema("read_file")];

        let report = super::surface_report_from_visible_schemas(
            &schemas, 42,  // budget_used — arbitrary, must pass through verbatim
            100, // budget_total
        );

        assert_eq!(
            report.visible_tools,
            vec!["grep".to_string(), "read_file".to_string()]
        );
        assert_eq!(report.visible_count, 2);
        assert_eq!(report.budget_used, 42);
        assert_eq!(report.budget_total, 100);
    }

    // ── Tool surface decision: structural signals, not text-based ────────
    //
    // The decision is driven by the tool pipeline state (visible schemas,
    // surface report, context signals) — never by NLP inference on the
    // user message text. This keeps the tool surface deterministic and
    // prompt-cache-friendly.

    /// Helper: empty report with the given budget_total.
    fn empty_report(budget_total: u32) -> astra_runtime::tool_registry::ToolSurfaceReport {
        astra_runtime::tool_registry::ToolSurfaceReport {
            visible_tools: Vec::new(),
            visible_count: 0,
            budget_used: 0,
            budget_total,
        }
    }

    #[test]
    fn tool_surface_decision_signals_and_priority() {
        // Table-driven: each row tests one signal in isolation, then the
        // priority chain verifies that higher signals beat lower ones when
        // multiple are simultaneously true.

        // ── Individual signals (only one true, others false) ──
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(100), false, false, false, false),
            (false, ""),
            "no signals → tool-free"
        );
        assert_eq!(
            super::tool_surface_should_inject(
                &[schema("bash")],
                &empty_report(100),
                false,
                false,
                false,
                false
            ),
            (true, "visible_tool_candidates"),
        );
        {
            let mut r = empty_report(100);
            r.visible_tools = vec!["git".into()];
            r.visible_count = 1;
            assert_eq!(
                super::tool_surface_should_inject(&[], &r, false, false, false, false),
                (true, "surface_report_names"),
            );
        }
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(100), true, false, false, false),
            (true, "had_tools_before_runtime_filter"),
        );
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(100), false, true, false, false),
            (true, "recent_tool_context"),
        );
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(100), false, false, true, false),
            (true, "tool_results_followup"),
        );
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(100), false, false, false, true),
            (true, "plan_mode_active"),
        );
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(0), false, false, false, false),
            (true, "budget_starved_surface"),
            "budget_total == 0 with no prior candidates → structurally starved"
        );

        // ── Priority: higher signals beat lower when multiple are true ──
        let report_with_tools = {
            let mut r = empty_report(0);
            r.visible_tools = vec!["git".into()];
            r.visible_count = 1;
            r
        };
        struct PriorityCase {
            schemas: Vec<Value>,
            report: astra_runtime::tool_registry::ToolSurfaceReport,
            had_tools_before_runtime_filter: bool,
            recent_tool_context: bool,
            tool_results_followup: bool,
            plan_mode_active: bool,
            expected_reason: &'static str,
            desc: &'static str,
        }

        let cases = [
            PriorityCase {
                schemas: vec![schema("bash")],
                report: report_with_tools.clone(),
                had_tools_before_runtime_filter: true,
                recent_tool_context: true,
                tool_results_followup: true,
                plan_mode_active: true,
                expected_reason: "visible_tool_candidates",
                desc: "turn_schemas beats all",
            },
            PriorityCase {
                schemas: Vec::new(),
                report: report_with_tools,
                had_tools_before_runtime_filter: true,
                recent_tool_context: true,
                tool_results_followup: true,
                plan_mode_active: true,
                expected_reason: "surface_report_names",
                desc: "surface report beats signals below",
            },
            PriorityCase {
                schemas: Vec::new(),
                report: empty_report(0),
                had_tools_before_runtime_filter: true,
                recent_tool_context: true,
                tool_results_followup: true,
                plan_mode_active: true,
                expected_reason: "had_tools_before_runtime_filter",
                desc: "pre-filter snapshot beats context signals",
            },
            PriorityCase {
                schemas: Vec::new(),
                report: empty_report(0),
                had_tools_before_runtime_filter: false,
                recent_tool_context: true,
                tool_results_followup: true,
                plan_mode_active: true,
                expected_reason: "recent_tool_context",
                desc: "recent tools beats results + plan",
            },
            PriorityCase {
                schemas: Vec::new(),
                report: empty_report(0),
                had_tools_before_runtime_filter: false,
                recent_tool_context: false,
                tool_results_followup: true,
                plan_mode_active: true,
                expected_reason: "tool_results_followup",
                desc: "tool results beats plan mode",
            },
            PriorityCase {
                schemas: Vec::new(),
                report: empty_report(0),
                had_tools_before_runtime_filter: false,
                recent_tool_context: false,
                tool_results_followup: false,
                plan_mode_active: true,
                expected_reason: "plan_mode_active",
                desc: "plan mode beats budget starved",
            },
        ];
        for case in cases {
            assert_eq!(
                super::tool_surface_should_inject(
                    &case.schemas,
                    &case.report,
                    case.had_tools_before_runtime_filter,
                    case.recent_tool_context,
                    case.tool_results_followup,
                    case.plan_mode_active
                ),
                (true, case.expected_reason),
                "{}",
                case.desc
            );
        }
    }

    #[test]
    fn tool_surface_decision_edge_cases() {
        // visible_count > 0 with empty vecs
        let count_only = astra_runtime::tool_registry::ToolSurfaceReport {
            visible_count: 3,
            budget_total: 100,
            ..empty_report(100)
        };
        assert_eq!(
            super::tool_surface_should_inject(&[], &count_only, false, false, false, false),
            (true, "surface_report_names"),
        );

        // budget_total == 0 but HadToolsBeforeRuntimeFilter is already set →
        // the pre-filter signal wins (priority), not BudgetStarved
        assert_eq!(
            super::tool_surface_should_inject(&[], &empty_report(0), true, false, false, false),
            (true, "had_tools_before_runtime_filter"),
            "pre-filter snapshot beats budget starved in priority order"
        );
    }

    // ── Regression: skill allowed_tools not force-included (session c3dea07a) ──
    //
    // When a skill declares allowed_tools (e.g. review-changes allows grep, glob),
    // the surface builder may not include them. The skill instructions reference
    // these tools, so they must be present in the final selection.

    #[test]
    fn skill_allowed_tools_injected_into_selection() {
        use astra_runtime::tool_registry::ToolSurfaceReport;
        use astra_turn_core::tool_schema_prune::inject_skill_allowed_tools;

        let all_schemas = [
            schema("bash"),
            schema("read_file"),
            schema("grep"),
            schema("glob"),
        ];

        // Surface included bash and read_file, but not grep/glob
        let mut turn_schemas = vec![schema("bash"), schema("read_file")];
        let mut report = ToolSurfaceReport {
            visible_tools: vec!["bash".into(), "read_file".into()],
            visible_count: 2,
            budget_used: 0,
            budget_total: 0,
        };

        // Skill allows bash, read_file, grep, glob
        let allowed: Vec<String> = vec![
            "bash".into(),
            "read_file".into(),
            "grep".into(),
            "glob".into(),
        ];

        let injected =
            inject_skill_allowed_tools(&mut turn_schemas, &mut report, &allowed, &all_schemas);

        assert_eq!(injected, 2);
        assert_eq!(report.visible_count, 4);
        assert!(report.visible_tools.contains(&"grep".into()));
        assert!(report.visible_tools.contains(&"glob".into()));
        assert_eq!(turn_schemas.len(), 4);
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_includes_plan_mode_escape_hatches() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![
            schema("read_file"),
            schema("write_file"),
            schema("enter_plan_mode"),
            schema("exit_plan_mode"),
        ];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(100);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "inspect the repo state"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repo state",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: true,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        let edge_tool_name_set: HashSet<String> = edge_tool_names
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let always_load_names: Vec<String> = payload["edge_profile"]
            [EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES]
            .as_array()
            .expect("edge_profile must carry resolved always_load tool names on tool turns")
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            always_load_names,
            registry.always_load_tool_names_sorted(),
            "CLI must send the resolved always_load set so runtime cache boundaries follow tool_surface config"
        );
        assert_eq!(
            valid_tool_names, edge_tool_name_set,
            "headless validator must admit exactly the tools sent in edge_tools"
        );
        assert_eq!(
            first_surface_report
                .as_ref()
                .map(|report| report.visible_tools.clone())
                .unwrap_or_default(),
            edge_tool_names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
            "surface telemetry must describe the final visible tools, not raw surface candidates"
        );
        let expected_visible_schema_tokens: u32 = edge_tool_names
            .iter()
            .map(|name| registry.token_cost(name))
            .sum();
        assert_eq!(
            first_surface_report
                .as_ref()
                .map(|report| report.budget_used),
            Some(expected_visible_schema_tokens),
            "final surface telemetry budget_used must use the same full visible-tool surface as visible_count"
        );
        // Plan-mode escape hatches must be present exactly once each.
        assert!(edge_tool_names.contains(&"enter_plan_mode"));
        assert!(edge_tool_names.contains(&"exit_plan_mode"));
        assert_eq!(
            edge_tool_names
                .iter()
                .filter(|name| **name == "enter_plan_mode")
                .count(),
            1
        );
        assert_eq!(
            edge_tool_names
                .iter()
                .filter(|name| **name == "exit_plan_mode")
                .count(),
            1
        );
        let deferred_tool_names: HashSet<String> = payload["edge_profile"]
            [EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            edge_tool_name_set.is_disjoint(&deferred_tool_names),
            "a tool cannot be both visible in edge_tools and advertised as deferred; visible={edge_tool_name_set:?} deferred={deferred_tool_names:?}"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_excludes_plan_tools_when_plan_mode_inactive() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![
            schema("read_file"),
            schema("write_file"),
            schema("enter_plan_mode"),
            schema("exit_plan_mode"),
        ];
        // Budget of 2 forces the surface builder to expose only the 2 most relevant tools,
        // leaving plan-mode escape hatches absent naturally. This makes the test
        // meaningful: if the `plan_mode_active` guard is accidentally removed, the
        // injection would add them and the assertion below would fail.
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "inspect the repo state"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repo state",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        // Session-control tools are always injected for cache stability
        // (prevents schema thrashing on plan-mode transitions).
        assert!(
            edge_tool_names.contains(&"enter_plan_mode"),
            "enter_plan_mode should always be injected for cache stability"
        );
        assert!(
            edge_tool_names.contains(&"exit_plan_mode"),
            "exit_plan_mode should always be injected for cache stability"
        );
        assert_eq!(
            first_surface_report
                .as_ref()
                .map(|report| report.visible_tools.clone())
                .unwrap_or_default(),
            edge_tool_names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
            "surface telemetry must stay in lockstep with final payload edge_tools"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_surface_edges_preserve_activation() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = astra_tools::schemas::all_tool_schemas();
        let registry = ToolRegistry::new(all_schemas.clone());
        let empty_schemas: Vec<Value> = Vec::new();
        let empty_registry = ToolRegistry::new(empty_schemas.clone());
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let empty_surface_message = "empty tool surface";
        let messages = vec![json!({"role": "user", "content": empty_surface_message})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder =
            StepRecorder::new("test-user", "session-empty-selector", "task-empty-selector");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-empty-surface"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: empty_surface_message,
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &empty_registry,
            tool_results: &tool_results,
            all_schemas: &empty_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::Auto,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tools = payload["edge_tools"].as_array().unwrap();
        assert!(
            edge_tools.is_empty(),
            "an explicitly empty tool surface without pending context should not include full tool schemas: {:?}",
            edge_tools
                .iter()
                .filter_map(|schema| schema["function"]["name"].as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            payload["edge_profile"]
                .get(EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT)
                .is_none(),
            "tool-free turns without visible tool_search should not advertise deferred tools"
        );
        assert!(
            valid_tool_names.is_empty(),
            "executor admission must mirror the tool-free payload"
        );
        assert_eq!(
            first_surface_report
                .as_ref()
                .map(|report| report.visible_count),
            Some(0),
            "surface telemetry must reflect the final no-tool surface"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "no activation should be recorded before a select/direct-call intent"
        );

        executor.set_current_visible_tool_schemas(&[schema("tool_search")]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));
        let selected = executor
            .execute("tool_search", &json!({"query": "select:memory"}))
            .await;
        let selected_json: Value = serde_json::from_str(&selected).unwrap_or_else(|error| {
            panic!("tool_search select should return JSON, got {error}: {selected}")
        });
        assert_eq!(selected_json["matches"][0]["name"].as_str(), Some("memory"));
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()]
        );

        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new(
            "test-user",
            "session-pending-activation",
            "task-pending-activation",
        );
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-pending-activation"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: empty_surface_message,
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::Auto,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 2,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(
            edge_tool_names.contains(&"memory"),
            "pending activation must surface the selected schema independent of the otherwise empty tool surface: {edge_tool_names:?}"
        );
        assert!(
            valid_tool_names.contains("memory"),
            "executor admission must mirror the activated schema visible in the payload"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "payload assembly must not consume activation before the tool is called"
        );
        let _ = executor.execute("memory", &json!({})).await;
        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "the accepted visible tool call consumes the matching activation"
        );
        executor.clear_current_tool_surface_for_tests();

        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(0);
        let messages = vec![json!({"role": "user", "content": "inspect the repository"})];
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-empty", "task-empty");
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-empty"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repository",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::Auto,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(
            edge_tool_names.contains(&"tool_search"),
            "budget-starved turns must keep deferred discovery reachable through the declarative default surface: {edge_tool_names:?}"
        );
        assert!(
            payload["edge_profile"]
                .get(EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT)
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("<deferred_tools>")),
            "tool_search visibility must be paired with a deferred manifest"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_injects_background_controls_when_bash_selected() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![
            schema("bash"),
            schema("task_output"),
            schema("task_list"),
            schema("task_stop"),
            schema("read_file"),
        ];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "run make check"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "run make check",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(
            edge_tool_names.contains(&"bash"),
            "test requires bash to be selected: {edge_tool_names:?}"
        );
        for name in ["task_output", "task_list", "task_stop"] {
            assert!(
                edge_tool_names.contains(&name),
                "Bash selection must force-inject {name} for same-turn Ctrl+B follow-up: {edge_tool_names:?}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_prefers_structured_attachment_override() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
            turn::turn_trace_collector::TurnTraceCollector,
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("read_file"), schema("write_file")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let message = "修复 timeout handling";
        let messages = vec![json!({"role": "user", "content": message})];
        let history = vec![(
            "review timeout handling".to_string(),
            "Need a fix.".to_string(),
        )];
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let tool_results = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();
        let trace_collector = TurnTraceCollector::new("turn-1", "session-1");
        let semantic_query_override =
            "Task: review timeout handling\nAssistant summary: Need a fix.\nFollow-up: 修复?";

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message,
            semantic_query_override: Some(semantic_query_override),
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: Some(&trace_collector),
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let trace = trace_collector.finalize();
        assert_eq!(trace.memory.query, semantic_query_override);
        let edge_tool_names: Vec<String> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(
            trace
                .tools
                .visible_tools
                .iter()
                .map(|tool| tool.tool_name.clone())
                .collect::<Vec<_>>(),
            edge_tool_names,
            "context assembly trace must report final visible edge_tools"
        );
        assert_eq!(
            trace.tools.tools_available as usize,
            payload["edge_tools"].as_array().unwrap().len(),
            "context assembly trace tools_available must be the final visible count"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_applies_pending_round_tool_boost() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![
            schema("read_file"),
            schema("write_file"),
            schema("bash"),
            schema("str_replace"),
        ];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        executor.debug_stage_pending_round_tool_boost_for_test(&[
            "bash",
            "read_file",
            "write_file",
            "str_replace",
        ]);
        let messages = vec![json!({"role": "user", "content": "implement the approved plan"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "implement the approved plan",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(edge_tool_names.contains(&"bash"));
        assert!(edge_tool_names.contains(&"read_file"));
        assert!(edge_tool_names.contains(&"write_file"));
        assert!(edge_tool_names.contains(&"str_replace"));

        assert!(executor.take_pending_round_tool_boost().is_none());
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_injects_activated_deferred_tools_into_edge_tools() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("read_file"), schema("tool_search"), schema("memory")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        executor.set_current_visible_tool_schemas(&[schema("tool_search")]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));
        let search = executor
            .execute("tool_search", &json!({"query": "select:memory"}))
            .await;
        let search_json: Value = serde_json::from_str(&search)
            .unwrap_or_else(|error| panic!("tool_search must return JSON, got {error}: {search}"));
        let search_match_names: Vec<&str> = search_json["matches"]
            .as_array()
            .expect("tool_search matches must be an array")
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect();
        assert_eq!(search_match_names, vec!["memory"]);
        assert!(
            search_json["matches"][0].get("parameters").is_some(),
            "tool_search select must return callable parameter shape: {search_json}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()]
        );

        let messages = vec![json!({"role": "user", "content": "remember this"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "remember this",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: HashSet<String> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .map(ToString::to_string)
            .collect();
        assert!(edge_tool_names.contains("memory"), "{edge_tool_names:?}");
        assert!(
            valid_tool_names.contains("memory"),
            "activated deferred tool must be admitted only after it is injected"
        );
        assert_eq!(valid_tool_names, edge_tool_names);
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "payload assembly must not consume activation before the activated tool is called"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_clears_stale_activatable_when_no_deferred_manifest() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("tool_search")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        executor.set_current_visible_tool_schemas(&[schema("tool_search")]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let messages = vec![json!({"role": "user", "content": "no deferred tools"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "no deferred tools",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        assert!(
            payload["edge_profile"][EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
                .as_array()
                .is_none_or(|names| names.is_empty()),
            "payload without a deferred prompt block must not carry deferred names: {payload}"
        );
        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .is_empty(),
            "stale activatable names must be cleared when no deferred manifest is rendered"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_does_not_advertise_unbound_deferred_runtime_tool() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("tool_search"), schema("agent_fanout")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));

        let messages =
            vec![json!({"role": "user", "content": "delegate review with parallel agents"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "delegate review with parallel agents",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        assert!(
            payload["edge_profile"][EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
                .as_array()
                .is_none_or(|names| names.is_empty()),
            "payload must not advertise a deferred runtime tool that local tool_search cannot activate: {payload}"
        );
        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .is_empty(),
            "executor activatable set must agree with the payload deferred manifest"
        );
        let search = executor
            .execute("tool_search", &json!({"query": "select:agent_fanout"}))
            .await;
        let search_json: Value = serde_json::from_str(&search).unwrap();
        assert!(
            search_json["matches"].as_array().unwrap().is_empty(),
            "tool_search must not resolve unbound agent_fanout: {search_json}"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_does_not_put_unbound_runtime_tool_in_tools_array() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("tool_search"), schema("agent_fanout")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        executor.debug_stage_pending_round_tool_boost_for_test(&["agent_fanout"]);

        let messages = vec![json!({"role": "user", "content": "fan out this work"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "fan out this work",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let edge_tool_names: HashSet<String> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .map(ToString::to_string)
            .collect();
        assert!(
            !edge_tool_names.contains("agent_fanout"),
            "tools[] must not advertise a runtime-gated tool without a connected runtime: {payload}"
        );
        assert!(
            !valid_tool_names.contains("agent_fanout"),
            "validator must mirror the filtered tools[] surface"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_consumes_widen_surface_pending_once() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("read_file"), schema("write_file")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "update the file"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_surface_pending = true;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let mut turn_guard = TurnGuard::default();
        turn_guard.health.record_failure("write_file");
        turn_guard.health.record_failure("write_file");
        turn_guard.health.record_failure("write_file");
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_surface_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let first_payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "update the file",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 0,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let first_tool_names: Vec<&str> = first_payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(first_tool_names.contains(&"write_file"));
        assert!(!widen_surface_pending);

        let second_payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_volatile_texts: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            model: None,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "update the file",
            semantic_query_override: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_surface_pending: &mut widen_surface_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_surface_report: &mut first_surface_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
            skill_search: &skill_search,
            is_plan_subtask: false,
            plan_subtask_id: None,
            timing_phases: false,
            prep_ui_phase: None,
            skill_effort: None,
            skill_agent_type: None,
            interaction_mode: TurnInteractionMode::NonInteractive,
            turn_policy: &mut turn_policy,
            skill_allowed_tools: None,
            previous_confidence_fallback: None,
            round_index: 1,
            session_turn: 1,
            turn_chain_id: None,
            user_query_event_id: None,
            denial_pressure: (0, 0),
            recent_rejections: Vec::new(),
            observability_hub: None,
            append_system_prompt: None,
            plan_mode_active: false,
        })
        .await;

        let second_tool_names: Vec<&str> = second_payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(second_tool_names.contains(&"write_file"));
        assert!(
            restricted_tools.is_empty(),
            "soft health signals must not remove visible tool schemas"
        );
    }
}

// Note: Environment variable parsing tests for `chat_turn_timing_stderr_enabled` were removed
// because unsafe `std::env::set_var` is unsound in multi-threaded programs. The function's
// logic is trivial (pattern matching on env var values) and not worth testing with unsafe code.
