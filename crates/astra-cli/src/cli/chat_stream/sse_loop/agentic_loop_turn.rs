//! Outbound `/chat/stream` payload preparation + fetch + SSE consume.
//!
//! The heavy orchestrator (`run_agentic_loop_iteration`) has been replaced by
//! the runtime's [`run_agentic_loop_with_host`]; this module now only exposes
//! `fetch_chat_turn_sse` for use by the CLI Server-admission adapter.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use astra_config::user_profile::{Scenario, TurnIntent, WorkspaceMutationIntent};
use astra_runtime::{
    pipeline::step_recorder::StepRecorder,
    prompts,
    tool_registry::{self, ToolRegistry},
    turn::agentic_loop::host::{TurnInteractionMode, TurnInteractionPolicy, VolatileInjection},
    turn::agentic_prepare_payload::attach_filtered_edge_tools_to_payload,
    turn::agentic_turn_telemetry::{
        capture_first_surface_report_if_empty, record_first_latency_ms_since,
    },
    turn::chat_turn_api_error::{
        CHAT_TURN_POST_MAX_RETRIES, chat_turn_http_error_with_compact_body,
    },
    turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn_with_input_budget,
    turn::chat_turn_edge_profile::{
        EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES, EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES,
        EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES,
        EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW, EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT,
        read_git_branch_abbrev,
    },
    turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, attach_turn_identity, chat_turn_base_payload,
        merge_active_skills_into_edge_profile, merge_edge_profile_extensions,
        set_payload_tool_results_if_non_empty,
    },
    turn::chat_turn_step_plan::record_agentic_step_plan_after_payload_prep,
    turn::prepare_turn_explain_text::restricted_tools_explain_text,
    turn::tool_schema_prune::retain_invoked_tool_schemas,
    turn::turn_guard::TurnGuard,
};
use astra_turn_core::tool::schema::tool_schema_name;
use astra_turn_core::tool_registry_report::ToolSelectionReport;
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

/// Session-control tools injected unconditionally to prevent schema thrashing.
/// Their combined cost is < 200 tokens but toggling them on/off breaks prompt
/// caching at every plan-mode transition or tool surface variance.
const CACHE_STABLE_SESSION_TOOLS: &[&str] =
    &["enter_plan_mode", "exit_plan_mode", "compress_context"];
const FIRST_CLASS_BROWSER_TOOLS: &[&str] = &["web_fetch", "web_search"];

fn inject_first_class_browser_tools(
    turn_schemas: &mut Vec<Value>,
    surface_report: &mut ToolSelectionReport,
    all_schemas: &[Value],
) {
    // Browser access is a first-class interactive capability. Keep it on the
    // stable tool-bearing surface instead of making an ordinary web/task turn
    // spend a discovery round. Registry availability and runtime binding still
    // decide whether a schema survives; this helper does not inspect user text.
    astra_turn_core::tool_schema_prune::inject_required_tool_names(
        turn_schemas,
        surface_report,
        FIRST_CLASS_BROWSER_TOOLS,
        all_schemas,
    );
}

/// Per-phase stderr timings for `/chat/stream`. Disabled — use `RUST_LOG=debug` instead.
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

/// Updates the live stderr prep line (`Ns  Phase… ⠿`, braille animates at end) for normal chat.
fn touch_prep_ui_phase(phase: &Option<ChatPrepPhaseLabel>, label: &str) {
    if let Some(a) = phase
        && let Ok(mut w) = a.write()
    {
        *w = label.to_string();
    }
}

fn trace_token_count_u32(value: u64, field: &'static str) -> u32 {
    match u32::try_from(value) {
        Ok(value) => value,
        Err(_) => {
            tracing::warn!(
                target: "astra_cli::turn_trace",
                field,
                value,
                max = u32::MAX,
                "token trace value exceeded u32 range; saturating"
            );
            u32::MAX
        }
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

fn retained_history_messages(messages: &[Value]) -> &[Value] {
    match messages.split_last() {
        Some((last, history)) if last.get("role").and_then(Value::as_str) == Some("user") => {
            history
        }
        _ => messages,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CrossSessionMemoryProjection {
    contents: Vec<String>,
    ranked: Vec<(String, f64)>,
    preferred_repos: Vec<String>,
    feedback_ids: Vec<String>,
}

fn project_cross_session_memory_hits(
    query: &str,
    memory_hits: &[astra_tools::memoria::BoostSearchHit],
) -> CrossSessionMemoryProjection {
    let contents: Vec<String> = memory_hits.iter().map(|hit| hit.content.clone()).collect();
    let ranked = if contents.is_empty() {
        Vec::new()
    } else {
        astra_turn_core::retrieval::rank_memory_results(query, &contents)
    };
    let preferred_repos = contents
        .iter()
        .flat_map(|content| extract_repos_from_memory(content))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let feedback_ids = memory_hits
        .iter()
        .filter_map(|hit| hit.memory_id.clone())
        .collect();
    CrossSessionMemoryProjection {
        contents,
        ranked,
        preferred_repos,
        feedback_ids,
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
        let preview = retained_history_preview(&role, &msg_content(message));

        if turns.is_empty() || role == "user" {
            turns.push(astra_turn_core::context_assembly_trace::TurnRetention {
                turn_index: turns.len() as u32,
                role,
                tokens,
                has_tool_calls,
                content_preview: preview,
            });
            continue;
        }

        if let Some(turn) = turns.last_mut() {
            turn.tokens += tokens;
            turn.has_tool_calls |= has_tool_calls;
            if retained_turn_role_priority(&role) > retained_turn_role_priority(&turn.role) {
                turn.role = role;
            }
            if !preview.is_empty() {
                const MAX_GROUP_PREVIEW_CHARS: usize = 320;
                let joined = if turn.content_preview.is_empty() {
                    preview
                } else {
                    format!("{} · {preview}", turn.content_preview)
                };
                turn.content_preview = astra_turn_core::context_assembly_trace::preview_snippet(
                    &joined,
                    MAX_GROUP_PREVIEW_CHARS,
                );
            }
        }
    }

    turns
}

/// Bounded human evidence for the exact message group accounted for by the
/// context trace. This is observability data, not prompt input and not a UI
/// transcript join key.
fn retained_history_preview(role: &str, content: &str) -> String {
    let snippet = astra_turn_core::context_assembly_trace::preview_snippet(content, 180);
    if snippet.is_empty() {
        String::new()
    } else {
        format!("{role}: {snippet}")
    }
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
    pub first_selection_report: &'a mut Option<ToolSelectionReport>,
    pub first_budget_pressure: &'a mut f64,
    pub first_context_assembly_ms: &'a mut Option<u64>,
    pub all_selected_skills: &'a mut Vec<String>,
    /// Optional trace collector for observability (M1).
    pub trace_collector: Option<&'a astra_runtime::turn::turn_trace_collector::TurnTraceCollector>,
}

struct PrepareChatTurnRequest<'a> {
    messages: &'a [Value],
    runtime_required_texts: &'a [String],
    active_system_skills: &'a [String],
    runtime_volatile_texts: &'a [String],
    runtime_volatile_injections: &'a [VolatileInjection],
    ephemeral_prefix: Option<&'a Value>,
    current_session_id: Option<&'a str>,
    offering_id: Option<&'a str>,
    model: Option<&'a str>,
    context_window_tokens: u32,
    effective_input_budget_tokens: u64,
    explain: AgenticChatExplainFlags,
    project_root: &'a Path,
    message: &'a str,
    user_intent: &'a str,
    semantic_query_override: Option<&'a str>,
    turn_intent: Option<&'a TurnIntent>,
    history: &'a [(String, String)],
    recent_tools: &'a [String],
    executor: Arc<ToolExecutor>,

    registry: &'a ToolRegistry,
    tool_results: &'a [Value],
    all_schemas: &'a [Value],
    valid_tool_names: &'a mut HashSet<String>,
    turn_guard: &'a TurnGuard,
    restricted_tools: &'a mut HashSet<String>,
    widen_selection_pending: &'a mut bool,
    step_recorder: &'a mut StepRecorder,
    file_context: &'a [String],
    assembly_start: Instant,
    telem: PrepareTurnTelemetry<'a>,
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
    /// so the per-turn SelfModel ingest can read recent feedback signals.
    observability_hub: Option<&'a Arc<astra_runtime::observability::ObservabilityHub>>,
    append_system_prompt: Option<&'a str>,
    /// Whether the current permission mode is `Plan`. When true the schema-
    /// preparation step adds every mutating tool to `restricted_tools` so the
    /// model only sees read-only + plan-control tools (`exit_plan_mode` etc.).
    plan_mode_active: bool,
    /// Pre-formatted lessons text from `session_lessons_snapshot()`.
    /// Injected into `edge_profile` as `lessons_text` for the bridge/assembler.
    pub lessons_text: Option<&'a str>,
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

fn enabled_optional_tool_names_from_schemas<'a>(
    schemas: impl IntoIterator<Item = &'a Value>,
) -> Vec<String> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut names = schemas
        .into_iter()
        .filter_map(tool_schema_name)
        .filter(|name| {
            registry
                .get(name)
                .is_some_and(astra_runtime_env::ToolSpec::requires_explicit_user_enablement)
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn surface_report_from_visible_schemas(
    schemas: &[Value],
    schema_budget_used: u32,
    schema_budget_total: u32,
) -> ToolSelectionReport {
    let visible_tools: Vec<String> = schemas
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect();
    ToolSelectionReport {
        visible_count: visible_tools.len() as u32,
        visible_tools,
        schema_budget_used,
        schema_budget_total,
    }
}

fn runtime_filter_turn_schemas_and_report(
    executor: &crate::edge_tools::ToolExecutor,
    turn_schemas: &mut Vec<Value>,
    surface_report: &mut ToolSelectionReport,
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

fn surface_report_has_visible_tools(report: &ToolSelectionReport) -> bool {
    !report.visible_tools.is_empty() || report.visible_count > 0
}

/// Priority-ordered check chain: first true signal wins. Returns
/// `(should_inject_tools, reason)` where `reason` is for `tracing::trace!`
/// observability only — never branched on by downstream code.
fn tool_surface_should_inject(
    turn_schemas: &[Value],
    surface_report: &ToolSelectionReport,
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
    if surface_report.schema_budget_total == 0 {
        return (true, "budget_starved_surface");
    }
    (false, "")
}

fn chat_turn_budget_pressure(
    messages: &[Value],
    registry: &ToolRegistry,
    effective_input_budget_tokens: u64,
) -> f64 {
    let schema_tokens = registry.total_always_load_token_cost();
    budget_pressure_for_chat_turn_with_input_budget(
        messages,
        schema_tokens as usize,
        effective_input_budget_tokens,
    )
}

struct PreparedChatTurnPayload {
    payload: Value,
    context_window_estimate: astra_turn_types::ContextWindowUsage,
    /// Exact token cost of the schemas actually sent in this request. The
    /// loop carries it into the next compaction decision, including deferred
    /// schemas materialized from retained conversation context.
    pinned_tool_schema_tokens: u64,
}

impl std::fmt::Display for PreparedChatTurnPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.payload.fmt(f)
    }
}

impl Deref for PreparedChatTurnPayload {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

/// Project CLI-owned capability facts into the sole Server-loop admission
/// contract. Conversation history, model rounds, tool results, and canonical
/// turn identity deliberately do not cross this boundary: the Server restores
/// and advances those authorities itself.
pub(crate) fn server_loop_admission_payload(
    prepared: &Value,
    message: &str,
    explain: bool,
) -> Result<Value, &'static str> {
    server_loop_admission_payload_with_execution_time_budget(prepared, message, explain, None)
}

fn server_loop_admission_payload_with_execution_time_budget(
    prepared: &Value,
    message: &str,
    explain: bool,
    execution_time_budget: Option<astra_services::runs::ExecutionTimeBudget>,
) -> Result<Value, &'static str> {
    let source = prepared
        .as_object()
        .ok_or("prepared developer loop payload must be an object")?;
    let required = |field: &'static str| {
        source
            .get(field)
            .cloned()
            .ok_or("prepared developer loop payload is missing a required field")
    };
    let mut context = serde_json::Map::new();
    if let Some(value) = source.get("edge_tools") {
        context.insert("edge_tools".to_string(), value.clone());
    }
    if let Some(value) = source.get("edge_profile") {
        context.insert("edge_profile".to_string(), value.clone());
    }
    if let Some(value) = source.get("edge_skills") {
        context.insert("edge_skills".to_string(), value.clone());
    }

    // `/chat/stream` is an active CLI-to-Server execution channel, not an
    // offline registration hint. Project that fact into the typed execution
    // binding contract so prompt admission, dispatch, child inheritance, and
    // observability all resolve the same provider.
    let edge_executor_id = source
        .get("edge_executor_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("prepared developer loop payload has no executable edge identity")?;
    let edge_profile = source
        .get("edge_profile")
        .and_then(Value::as_object)
        .ok_or("prepared developer loop payload has no edge workspace profile")?;
    let workspace_root = edge_profile
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("prepared developer loop payload has no edge workspace root")?;
    let workspace_display_name = edge_profile
        .get("display_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("CLI workspace");
    let workspace_authority = match edge_profile.get("authority") {
        None => "read_write",
        Some(Value::String(authority)) if authority == "read_only" => "read_only",
        Some(Value::String(authority)) if authority == "read_write" => "read_write",
        Some(Value::String(authority)) if authority == "none" => "none",
        Some(_) => return Err("prepared developer loop payload has invalid workspace authority"),
    };
    for field in [
        "thinking",
        "effort",
        "agent_type",
        "rollback_on_failure",
        "rollback_boundary",
    ] {
        if let Some(value) = source.get(field) {
            context.insert(field.to_string(), value.clone());
        }
    }

    let mut request = serde_json::Map::from_iter([
        ("message".to_string(), Value::String(message.to_string())),
        ("model_selection".to_string(), required("model_selection")?),
        (
            "edge_executor_id".to_string(),
            Value::String(edge_executor_id.to_string()),
        ),
        (
            "workspace_binding".to_string(),
            serde_json::json!({
                "kind": "edge_workspace",
                "display_name": workspace_display_name,
                "root": workspace_root,
                "source": {"kind": "edge_path", "path": workspace_root},
                "authority": workspace_authority,
            }),
        ),
        (
            "executor_binding".to_string(),
            serde_json::json!({
                "kind": "edge_agent",
                "executor_id": edge_executor_id,
                "display_name": workspace_display_name,
                "transport": "edge_ledger",
                "status": "online",
            }),
        ),
        ("capabilities".to_string(), required("capabilities")?),
        ("explain".to_string(), Value::Bool(explain)),
        ("interactive_client".to_string(), Value::Bool(true)),
        (
            "execution_policy".to_string(),
            // Use one typed semantic admission at the start of an unbound
            // conversation. The server skips it once Work is bound, so later
            // rounds do not pay a second model request or churn the cache.
            // The decision and initial graph remain LLM-driven; no client
            // prompt matcher is used.
            serde_json::json!({"turn_intent": "auto"}),
        ),
        ("context".to_string(), Value::Object(context)),
    ]);
    for field in [
        "user_intent",
        "runtime_system_prompt",
        "session_id",
        "agent_id",
        "interaction_mode",
        "enabled_tools",
        "plan_subtask_id",
        "is_plan_subtask",
    ] {
        if let Some(value) = source.get(field) {
            request.insert(field.to_string(), value.clone());
        }
    }
    if let Some(execution_time_budget) = execution_time_budget {
        request.insert(
            "execution_time_budget".to_string(),
            serde_json::to_value(execution_time_budget)
                .expect("ExecutionTimeBudget must remain JSON serializable"),
        );
    }
    Ok(Value::Object(request))
}

fn attach_typed_edge_skill_catalog(payload: &mut Value, listing: Option<&Value>) {
    let Some(edge_skills) = listing
        .and_then(|listing| listing.get("edge_skills"))
        .and_then(Value::as_array)
        .filter(|skills| !skills.is_empty())
    else {
        return;
    };
    payload["edge_skills"] = Value::Array(edge_skills.clone());
}

async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> PreparedChatTurnPayload {
    let timing = ctx.timing_phases;
    let mut mark = Instant::now();
    let prep_wall = mark;

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Starting…");

    let normalized_prompt =
        astra_turn_core::runtime_scaffolding::normalize_prompt_facing_runtime_messages(
            crate::cli::history_work::clone_json_history(
                astra_core::history_work::HistoryWorkSite::CliPromptNormalizationClone,
                ctx.messages,
            ),
        );
    let prompt_messages = normalized_prompt.messages;
    let mut runtime_required_texts = Vec::new();
    for text in normalized_prompt
        .required_runtime_texts
        .iter()
        .chain(ctx.runtime_required_texts.iter())
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
    {
        if !runtime_required_texts
            .iter()
            .any(|existing| existing == text)
        {
            runtime_required_texts.push(text.to_string());
        }
    }

    let git_branch = read_git_branch_abbrev();
    let requested_model = astra_core::model_override::normalize_model_override(ctx.model);
    let thinking_config = match requested_model {
        Some(m) => {
            let (_, cfg) = astra_turn_core::thinking_config::resolve_model_thinking(m);
            // Per-turn dampener: the model suffix encodes the user's CEILING
            // (e.g. `thinking:high`), not a command to burn that budget on every
            // turn regardless of content. Short read-only questions get a
            // capped effort — multi-step / modification turns pass through
            // unchanged. See `ThinkingConfig::scale_for_turn` for the policy.
            let signals = thinking_complexity_signals(ctx.message, ctx.turn_intent);
            cfg.scale_for_turn(signals)
        }
        None => astra_turn_core::thinking_config::ThinkingConfig::Off,
    };
    crate::cli::history_work::record_json_history(
        astra_core::history_work::HistoryWorkSite::CliPromptPayloadClone,
        &prompt_messages,
    );
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: &prompt_messages,
        user_intent: Some(ctx.user_intent),
        session_id: ctx.current_session_id,
        agent_id: Some("astra-cli"),
        inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
        round_index: ctx.round_index,
        offering_id: ctx.offering_id,
        interaction_mode: Some(ctx.interaction_mode.label()),
        explain_verbose: ctx.explain.explain_verbose,
        explain_on: ctx.explain.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: astra_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
        thinking: thinking_config,
    });

    // Carry only typed routing metadata across the trust boundary. Full skill
    // instructions remain client-owned and are returned only after the model
    // selects one through the `skill` tool. The server renders this catalog
    // with its canonical escaping/budget rules; raw client prompt fragments
    // are not treated as catalog authority.
    attach_typed_edge_skill_catalog(&mut payload, ctx.ephemeral_prefix);

    if ctx.message == astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE
        && ctx.semantic_query_override.is_some()
        && !runtime_required_texts.is_empty()
        && let Some(edge_profile) = payload
            .get_mut("edge_profile")
            .and_then(Value::as_object_mut)
    {
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_RECONCILIATION_TURN
                .to_string(),
            Value::Bool(true),
        );
    }

    if !runtime_required_texts.is_empty()
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS
                .to_string(),
            json!(runtime_required_texts),
        );
    }

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

    if let Some(value) =
        astra_runtime::turn::agentic_loop::host::runtime_volatile_injections_edge_profile_value(
            ctx.runtime_volatile_injections,
        )
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS
                .to_string(),
            value,
        );
    }

    // The rendered `ephemeral_prefix.content` remains local adapter state. It
    // is intentionally not copied into the request; only its typed catalog
    // projection above crosses the trust boundary.
    merge_active_skills_into_edge_profile(&mut payload, ctx.active_system_skills);

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

    let budget_pressure = chat_turn_budget_pressure(
        &prompt_messages,
        ctx.registry,
        ctx.effective_input_budget_tokens,
    );

    let memory_retrieval_decision =
        astra_turn_core::retrieval::decide_cross_session_memory_retrieval(
            ctx.message,
            ctx.semantic_query_override,
            !ctx.history.is_empty(),
        );
    let semantic_query_str = memory_retrieval_decision.query();
    {
        match memory_retrieval_decision {
            astra_turn_core::retrieval::CrossSessionMemoryDecision::Skip { query, reason } => {
                tracing::debug!(
                    ?reason,
                    "cross session memory retrieval skipped by structured decision"
                );
                if let Some(collector) = ctx.telem.trace_collector {
                    collector.record_memory_retrieval(query, 0, &[], 0);
                }
            }
            astra_turn_core::retrieval::CrossSessionMemoryDecision::Retrieve { query, top_k } => {
                let mem_start = Instant::now();
                let memory_hits = ctx.executor.memory_boost_search(query, top_k).await;
                let mem_latency_ms = mem_start.elapsed().as_millis() as u64;
                record_first_latency_ms_since(ctx.telem.first_memoria_ms, mem_start);

                let projection = project_cross_session_memory_hits(query, &memory_hits);
                if let Some(collector) = ctx.telem.trace_collector {
                    collector.record_memory_retrieval(
                        query,
                        projection.contents.len() as u32,
                        &projection.ranked,
                        mem_latency_ms,
                    );
                }

                if !projection.contents.is_empty() {
                    for repo in &projection.preferred_repos {
                        ctx.executor.add_preferred_repo(repo);
                    }
                    // Send "useful" feedback for retrieved memories (fire-and-forget)
                    ctx.executor.memory_feedback_useful(projection.feedback_ids);
                }
            }
        }
    }
    log_chat_turn_timing_phase(timing, "memory_boost_search", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Preparing tools…");

    let judged_domain_hints = ctx
        .turn_intent
        .and_then(|intent| intent.domain)
        .map(|domain| vec![domain.as_str().to_string()])
        .unwrap_or_default();
    // Consume the one-shot strategy/correction reset marker. Tool health is
    // advisory only and never mutates the hard schema restriction set.
    let _ = std::mem::take(ctx.widen_selection_pending);
    ctx.step_recorder
        .record_perceive(semantic_query_str, &[], &judged_domain_hints);

    // Skill activation is handled exclusively by the `skill` tool in the agentic loop
    // (see turn/skill_tool.rs + partition_and_execute_skills). The model decides when
    // to invoke skills by calling the tool, rather than having skills pre-injected by
    // the tool surface builder.

    let typed_tool_surface_allowed = match ctx.turn_intent {
        Some(intent) => intent.communicative_act.uses_tool_surface(),
        None => true,
    };
    let (turn_schemas, surface_report, surface_latency_ms) = {
        let sel_start = Instant::now();
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
        let budget = ctx.registry.default_schema_budget();
        let (mut schemas, mut report) = ctx
            .registry
            .build_turn_surface_with_report(ctx.turn_intent, budget);
        if typed_tool_surface_allowed && !ctx.tool_results.is_empty() {
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
    log_chat_turn_timing_phase(timing, "registry_load_schemas", &mut mark);

    // Force-inject any skill allowed_tools that the assembled surface missed.
    let mut turn_schemas = turn_schemas;
    let mut surface_report = surface_report;
    let mut activated_deferred_tool_names = Vec::new();
    if typed_tool_surface_allowed {
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
            // Materialize deferred tools selected in retained conversation
            // context. A successful call cannot revoke a schema that later turns
            // may still need; only reset or a real surface change may remove it.
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
                activated_deferred_tool_names = activated;
            }
        }
    }
    let had_tools_before_runtime_filter = runtime_filter_turn_schemas_and_report(
        ctx.executor.as_ref(),
        &mut turn_schemas,
        &mut surface_report,
    );
    // NOTE: `schema_budget_used` is intentionally NOT recomputed here. The
    // `surface_report_from_visible_schemas` call below is the single source
    // of truth for the final report's budget; any value set on the
    // intermediate `surface_report.schema_budget_used` would be overwritten and
    // never consumed. See test
    // `surface_report_from_visible_schemas_is_single_source_for_budget`.

    let (inject_tools, surface_reason) = if typed_tool_surface_allowed {
        tool_surface_should_inject(
            &turn_schemas,
            &surface_report,
            had_tools_before_runtime_filter,
            !ctx.recent_tools.is_empty(),
            !ctx.tool_results.is_empty(),
            ctx.plan_mode_active,
        )
    } else {
        (false, "typed_non_work_act")
    };
    tracing::trace!(
        target: "astra.tool_surface",
        reason = surface_reason,
        inject_tools,
        "chat turn tool surface decision"
    );
    if inject_tools {
        inject_first_class_browser_tools(&mut turn_schemas, &mut surface_report, ctx.all_schemas);
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

    // Plan mode is enforced at permission/tool preflight time, not by
    // mutating `restricted_tools` here. Keeping schema filtering out of
    // payload assembly avoids plan/default prompt-cache churn and prevents
    // stale hard restrictions from leaking into later turns.

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
    let eligible_provider_schemas = ctx
        .executor
        .runtime_bound_provider_owned_schemas_excluding(ctx.restricted_tools);
    // `enabled_tools` describes product capabilities already present on this
    // request's executable edge/provider surface. It is not inferred from
    // user prose and it does not approve an invocation: domain/effect
    // permissions still run at the tool boundary. Publishing this fact keeps
    // root, Work-item, and delegated child admission on one authority chain.
    let enabled_optional_tools = enabled_optional_tool_names_from_schemas(
        eligible_surface_schemas
            .iter()
            .chain(eligible_provider_schemas.iter()),
    );
    if let Some(root) = payload.as_object_mut() {
        root.insert("enabled_tools".to_string(), json!(enabled_optional_tools));
    }
    let tool_surface = tool_registry::surface::ToolSurface::build_excluding_visible(
        eligible_surface_schemas,
        &astra_config::runtime_config::RuntimeConfig::cached().tool_surface,
        &eligible_provider_schemas,
        &final_visible_tool_names,
    );
    let mut activatable_tool_names = HashSet::new();
    let mut omitted_deferred_tool_names: Vec<String> = Vec::new();
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
        && let Some(manifest) =
            tool_surface.deferred_manifest_with_context_window(Some(ctx.context_window_tokens))
    {
        omitted_deferred_tool_names = manifest.omitted_names.clone();
        activatable_tool_names = manifest.names.iter().cloned().collect();
        merge_edge_profile_extensions(
            &mut payload,
            &json!({
                EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT: manifest.text,
                EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW: manifest.context_window,
                EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES: manifest.names,
                EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES: manifest.omitted_names,
            }),
        );
    }
    let deferred_available = activatable_tool_names.len().min(u32::MAX as usize) as u32;
    ctx.executor
        .set_current_tool_surface(&final_visible_schemas, activatable_tool_names);
    // Telemetry truth: recompute token cost from the actual final visible
    // schemas. The intermediate report may include recommendation hints later
    // stripped by capability/interaction-mode filtering.
    // Final persisted reports must keep `visible_count`, `visible_tools`,
    // and `schema_budget_used` on the same full-visible-surface basis.
    let visible_tool_costs: Vec<(String, u32)> = final_visible_schemas
        .iter()
        .filter_map(|schema| {
            tool_schema_name(schema).map(|name| (name.to_string(), ctx.registry.token_cost(name)))
        })
        .collect();
    let visible_tool_tokens_total_u64: u64 = visible_tool_costs
        .iter()
        .map(|(_, cost)| u64::from(*cost))
        .sum();
    let visible_tool_tokens_total =
        trace_token_count_u32(visible_tool_tokens_total_u64, "visible_tool_tokens_total");
    let final_surface_report = surface_report_from_visible_schemas(
        &final_visible_schemas,
        visible_tool_tokens_total,
        surface_report.schema_budget_total,
    );
    // `first_selection_report` is turn-level telemetry and intentionally
    // remains immutable after round zero. Step telemetry is round-level and
    // must use this round's actual final surface.
    let current_surface_report = final_surface_report.clone();
    let final_visible_tool_names_for_trace = final_visible_tool_names.clone();
    *ctx.valid_tool_names = final_visible_tool_names;

    if let Some(collector) = ctx.telem.trace_collector {
        let mut deferred_active_tools: Vec<String> = activated_deferred_tool_names
            .into_iter()
            .filter(|name| final_visible_tool_names_for_trace.contains(name))
            .collect();
        deferred_active_tools.sort();
        collector.record_tool_surface_with_deferred(
            astra_runtime::turn::turn_trace_collector::ToolSurfaceDeferredInput {
                visible_tools: &final_surface_report.visible_tools,
                per_tool_costs: &visible_tool_costs,
                tools_available: final_visible_schemas.len() as u32,
                latency_ms: surface_latency_ms,
                deferred_active_tools: &deferred_active_tools,
                deferred_available,
                deferred_omitted_tools: &omitted_deferred_tool_names,
            },
        );
    }

    capture_first_surface_report_if_empty(
        ctx.telem.first_selection_report,
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
        Some(&current_surface_report),
        budget_pressure,
    );

    record_first_latency_ms_since(ctx.telem.first_context_assembly_ms, ctx.assembly_start);

    inject_runtime_turn_overrides(
        &mut payload,
        ctx.is_plan_subtask,
        ctx.plan_subtask_id,
        ctx.skill_effort.as_deref(),
        ctx.skill_agent_type.as_deref(),
    );
    let _ = attach_turn_identity(
        &mut payload,
        ctx.session_turn,
        ctx.turn_chain_id,
        ctx.user_query_event_id,
    );
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
                .map(|hub| hub.recent_feedback_signals())
                .unwrap_or_default();
            session.ingest_self_model_inputs(skills, tool_health_entries, scenario, recent_signals);

            // Injection-freshness observation is deferred to after the
            // turn's SSE stream finishes (see `post_turn_observe_injections`
            // in `server_admission_host.rs`). Observing here would fire before
            // the bridge has actually composed its bridge-generated
            // channels (memoria_prefetch, tool_round_guidance, volatile) and leave
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
        astra_runtime::recent_arg_hints::prompt_block_from_messages(&prompt_messages)
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("recent_arg_hints_text".to_string(), json!(hints_text));
    }
    // ─── Lessons: inject bootstrapped session lessons into edge_profile ───
    // Fixes the "signal channel blank" bug where lessons were loaded from
    // Memoria by `ensure_bootstrapped_lessons()` but never injected into the
    // prompt. Previously lessons only flowed into observability, not into
    // the LLM context.
    if let Some(ref lessons) = ctx.lessons_text
        && let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        ep_obj.insert("lessons_text".to_string(), json!(lessons));
    }
    // ─── Runtime context: keep it on the typed Server admission lane ───
    if let Some(extra) = ctx.append_system_prompt {
        if let Some(root) = payload.as_object_mut() {
            root.insert("runtime_system_prompt".to_string(), json!(extra));
        }
    }
    log_chat_turn_timing_phase(timing, "self_awareness_inject", &mut mark);

    // Injection-freshness observation happens AFTER the bridge's SSE
    // stream completes (see `post_turn_observe_injections` in
    // `server_admission_host.rs`), so we can merge the 5 bridge-generated
    // channels (captured via the `injection_freshness` SSE event into
    // `ChatTurnSseAccum.injection_fingerprints`) with the CLI-owned
    // `lessons` snapshot.

    // ─── Context-window estimate / trace collection ───────────────────────
    // The runtime owns the final system prompt, so this starts without it and
    // is amended by the `context_meta` SSE event before the model responds.
    let schema_tokens = trace_token_count_u32(visible_tool_tokens_total_u64, "tool_schema_tokens");
    let max_tokens = trace_token_count_u32(ctx.effective_input_budget_tokens, "max_tokens");
    let history_messages = retained_history_messages(&prompt_messages);
    let history_tokens_u64: u64 = history_messages
        .iter()
        .map(|m| prompts::estimate_str_tokens(&msg_content(m)) as u64)
        .sum();
    let history_tokens = trace_token_count_u32(history_tokens_u64, "history_tokens");
    let turns_retained = build_retained_history_turns(history_messages);
    let user_message_tokens_u64 = prompts::estimate_str_tokens(ctx.message) as u64;
    let user_message_tokens = trace_token_count_u32(user_message_tokens_u64, "user_message_tokens");
    let memory_tokens_u64 = 0u64;
    let memory_tokens = trace_token_count_u32(memory_tokens_u64, "memory_tokens");
    let estimated_total = trace_token_count_u32(
        history_tokens_u64
            + memory_tokens_u64
            + visible_tool_tokens_total_u64
            + user_message_tokens_u64,
        "estimated_total",
    );

    if let Some(collector) = ctx.telem.trace_collector {
        collector.set_history_retained(&turns_retained);

        collector.record_token_budget_estimate(
            0,
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

    PreparedChatTurnPayload {
        payload,
        context_window_estimate: astra_turn_types::ContextWindowUsage::estimated(
            u64::from(estimated_total),
            u64::from(max_tokens),
        ),
        pinned_tool_schema_tokens: visible_tool_tokens_total_u64,
    }
}

fn thinking_complexity_signals(
    message: &str,
    turn_intent: Option<&TurnIntent>,
) -> astra_turn_core::thinking_config::TurnComplexitySignals {
    let typed_lightweight = turn_intent.is_some_and(|intent| {
        intent.workspace_mutation == WorkspaceMutationIntent::ReadOnly
            && intent.requested_scenario == Some(Scenario::QuickAnswer)
    });
    let continues_current_objective = turn_intent.is_some_and(|intent| {
        matches!(
            intent.objective_relation,
            astra_turn_types::ObjectiveRelation::Continue
                | astra_turn_types::ObjectiveRelation::Refine
                | astra_turn_types::ObjectiveRelation::Correct
        )
    });
    astra_turn_core::thinking_config::TurnComplexitySignals {
        input_char_len: message.trim().chars().count(),
        typed_lightweight,
        continues_current_objective,
    }
}

fn inject_runtime_turn_overrides(
    payload: &mut Value,
    is_plan_subtask: bool,
    plan_subtask_id: Option<&str>,
    skill_effort: Option<&str>,
    skill_agent_type: Option<&str>,
) {
    let Some(root) = payload.as_object_mut() else {
        return;
    };

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

// Skill activation goes through the `skill` tool in the agentic loop, not
// hidden payload injection.

// ─── Fetch: payload → POST → consume_turn_sse ─────────────────────────────────

pub(crate) struct ChatTurnSseFetchRequest<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub auth_profile: Option<&'a str>,
    pub offering_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub context_window_tokens: u32,
    pub effective_input_budget_tokens: u64,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub user_intent: &'a str,
    pub semantic_query_override: Option<&'a str>,
    pub turn_intent: Option<&'a TurnIntent>,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: &'a Path,
    pub executor: Arc<ToolExecutor>,
    pub registry: &'a ToolRegistry,
    pub messages: &'a [Value],
    /// Runtime-owned per-turn control context that must reach the current model
    /// call but must stay out of user content and persisted prompt-facing
    /// history.
    pub runtime_required_texts: &'a [String],
    pub active_system_skills: &'a [String],
    /// Dynamic text from external/session sources. This remains distinct from
    /// runtime-owned typed injections below.
    pub runtime_volatile_texts: &'a [String],
    pub runtime_volatile_injections: &'a [VolatileInjection],
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
    /// Exact schema footprint from the last outbound payload. Updated after
    /// preparing this request so the next loop iteration compacts against the
    /// same tool surface that the model actually saw.
    pub pinned_tool_schema_tokens: &'a mut u64,
    pub turn_guard: &'a astra_turn_core::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub widen_selection_pending: &'a mut bool,
    pub step_recorder: &'a mut StepRecorder,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
    pub perm_manager: &'a mut PermissionManager,
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
    /// Strict machine observation stream for one-shot `stream-json`.
    pub stream_json_emitter:
        Option<std::sync::Arc<crate::cli::stream::stream_json::StreamJsonEmitter>>,
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
    /// Cross-turn tool output cache retained by the CLI admission adapter.
    pub tool_cache: &'a mut crate::cli::stream::stream_render::EdgeToolCache,
    /// Fallback from previous turn's confidence diagnosis for broadening.
    pub previous_confidence_fallback:
        Option<astra_turn_core::confidence_contract::ConfidenceFallback>,
    /// Current agentic loop round (0-based). Sent to bridge for tool round directives.
    pub round_index: u32,
    pub session_turn: u32,
    pub turn_chain_id: Option<&'a str>,
    pub user_query_event_id: Option<&'a str>,
    /// Optional shared observability hub for reading recent feedback signals
    /// window when publishing SelfModel inputs. Threaded through so the
    /// per-turn ingest can attach `recent_signals` to the session without
    /// needing a global singleton.
    pub observability_hub: Option<&'a Arc<astra_runtime::observability::ObservabilityHub>>,
    pub incremental_state: Option<Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
    pub request_session_execution_lease:
        Option<Arc<crate::cli::session::session_execution_lease::RequestSessionExecutionLease>>,
    pub append_system_prompt: Option<&'a str>,
    pub execution_time_budget: Option<&'a crate::cli::chat_stream::ExecutionTimeBudgetClock>,
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

/// Build a Server-loop admission (with optional prep line), POST `/chat/stream`,
/// and return the response plus prep guard.
///
/// The caller must drop [`ChatTurnPrepLineGuard`] when entering SSE consume (`consume_turn_sse`)
/// or on early error after reading the body, so the stderr status line stays through TTFB.
struct ChatTurnPostPayloadRequest<'a> {
    api: &'a astra_thin_client::ThinClient,
    token: &'a str,
    quiet: bool,
    ui: &'a ChatTurnSseFetchUi,
    stream_event_tx: Option<&'a crate::cli::chat_stream::StreamEventTx>,
    stream_json_emitter:
        Option<&'a std::sync::Arc<crate::cli::stream::stream_json::StreamJsonEmitter>>,
    execution_time_budget_clock: Option<&'a crate::cli::chat_stream::ExecutionTimeBudgetClock>,
    prepare: PrepareChatTurnRequest<'a>,
}

async fn chat_turn_post_payload_after_prepare(
    request: ChatTurnPostPayloadRequest<'_>,
) -> Result<
    (
        astra_thin_client::HttpResponse,
        ChatTurnPrepLineGuard,
        u64,
        Option<crate::cli::stream::stream_json::StreamJsonExchange>,
    ),
    String,
> {
    let ChatTurnPostPayloadRequest {
        api,
        token,
        quiet,
        ui,
        stream_event_tx,
        stream_json_emitter,
        execution_time_budget_clock,
        prepare,
    } = request;
    let prep_line = ChatTurnPrepLineGuard::maybe_start(ui.show_prep_line, ui.prep_ui_phase.clone());
    let (current_session_id, session_turn, round_index) = (
        prepare.current_session_id,
        prepare.session_turn,
        prepare.round_index,
    );
    let server_message = prepare.message.to_string();
    let server_explain = prepare.explain.explain_on || prepare.explain.explain_verbose;
    let prepared = prepare_chat_turn_payload(prepare).await;
    let execution_time_budget = execution_time_budget_clock.map(|budget| budget.remaining());
    let server_payload = server_loop_admission_payload_with_execution_time_budget(
        &prepared.payload,
        &server_message,
        server_explain,
        execution_time_budget,
    )
    .map_err(str::to_string)?;

    if let Some(tx) = stream_event_tx {
        let _ = tx.try_send(
            crate::cli::chat_stream::StreamEvent::ContextWindowEstimated(
                prepared.context_window_estimate,
            ),
        );
    }

    touch_prep_ui_phase(&ui.prep_ui_phase, "Sending…");
    // `exchange_started` is the logical Server-loop request boundary. The
    // thin client may perform internal 429 transport retries, which are not
    // exposed as separate exchanges. If the request fails or returns a
    // non-success status, this observer is dropped without an
    // `exchange_finished`; only a protocol `[DONE]` can close it.
    let stream_json_exchange = match stream_json_emitter {
        Some(emitter) => {
            Some(emitter.start_exchange(current_session_id, session_turn, round_index)?)
        }
        None => None,
    };
    let http_mark = Instant::now();
    let resp = api
        .post_developer_loop_retry_429(token, &server_payload, CHAT_TURN_POST_MAX_RETRIES, quiet)
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
    Ok((
        resp,
        prep_line,
        prepared.pinned_tool_schema_tokens,
        stream_json_exchange,
    ))
}

pub(crate) async fn fetch_chat_turn_sse(
    ctx: ChatTurnSseFetchRequest<'_>,
) -> Result<TurnResult, String> {
    let ChatTurnSseFetchRequest {
        api,
        token,
        auth_profile,
        offering_id,
        model,
        context_window_tokens,
        effective_input_budget_tokens,
        explain,
        render_md,
        term_width,
        render_policy,
        message,
        user_intent,
        turn_intent,
        history,
        recent_tools,
        project_root,
        executor,
        registry,
        messages,
        runtime_required_texts,
        active_system_skills,
        runtime_volatile_texts,
        runtime_volatile_injections,
        ephemeral_prefix,
        current_session_id,
        tool_results,
        all_schemas,
        valid_tool_names,
        pinned_tool_schema_tokens,
        turn_guard,
        restricted_tools,
        widen_selection_pending,
        step_recorder,
        file_context,
        assembly_start,
        telem,
        perm_manager,
        pre_clear_lines,
        is_plan_subtask,
        plan_subtask_id,
        cancel_token,
        plan_assemble_line_release,
        stream_event_tx,
        stream_json_emitter,
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
        request_session_execution_lease,
        append_system_prompt,
        execution_time_budget,
        semantic_query_override,
        ..
    } = ctx;

    let ui = chat_turn_sse_fetch_ui(render_policy, plan_assemble_line_release.as_ref());

    // Compute lessons text from Memoria-bootstrapped session lessons.
    // Format: "kind:trigger_signal:action" per lesson, pipe-joined.
    // Mirrors the format used in the Server-admission host's observability path.
    let lessons_text: Option<String> = {
        let lessons = executor.session_lessons_snapshot();
        if lessons.is_empty() {
            None
        } else {
            Some(
                lessons
                    .iter()
                    .map(|l| format!("{}:{}:{}", l.kind.as_str(), l.trigger_signal, l.action))
                    .collect::<Vec<_>>()
                    .join("|"),
            )
        }
    };
    let lessons_text_ref: Option<&str> = lessons_text.as_deref();

    let (resp, prep_line, prepared_schema_tokens, stream_json_exchange) =
        chat_turn_post_payload_after_prepare(ChatTurnPostPayloadRequest {
            api,
            token,
            quiet: render_policy.is_silent(),
            ui: &ui,
            stream_event_tx: stream_event_tx.as_ref(),
            stream_json_emitter: stream_json_emitter.as_ref(),
            execution_time_budget_clock: execution_time_budget,
            prepare: PrepareChatTurnRequest {
                messages,
                runtime_required_texts,
                active_system_skills,
                runtime_volatile_texts,
                runtime_volatile_injections,
                ephemeral_prefix,
                current_session_id,
                offering_id,
                model,
                context_window_tokens,
                effective_input_budget_tokens,
                explain: AgenticChatExplainFlags::from_explain_ui_mode(match explain {
                    ExplainMode::Off => AgenticExplainUiMode::Off,
                    ExplainMode::On => AgenticExplainUiMode::On,
                    ExplainMode::Verbose => AgenticExplainUiMode::Verbose,
                }),
                project_root,
                message,
                user_intent,
                semantic_query_override,
                turn_intent,
                history,
                recent_tools,
                executor: Arc::clone(&executor),
                registry,
                tool_results,
                all_schemas,
                valid_tool_names,
                turn_guard,
                restricted_tools,
                widen_selection_pending,
                step_recorder,
                file_context,
                assembly_start,
                telem,
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
                lessons_text: lessons_text_ref,
            },
        })
        .await?;

    *pinned_tool_schema_tokens = prepared_schema_tokens;

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
        request_session_execution_lease,
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
        stream_json_exchange,
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
        PrepareChatTurnRequest, PrepareTurnTelemetry, attach_typed_edge_skill_catalog,
        build_retained_history_turns, chat_turn_budget_pressure, inject_runtime_turn_overrides,
        msg_content, prepare_chat_turn_payload, project_cross_session_memory_hits,
        retained_history_messages, runtime_filter_turn_schemas_and_report,
        server_loop_admission_payload, server_loop_admission_payload_with_execution_time_budget,
        surface_report_from_visible_schemas, thinking_complexity_signals,
    };
    use astra_config::user_profile::{Scenario, TurnIntent, WorkspaceMutationIntent};
    use astra_runtime::turn::agentic_loop::host::{
        ASK_USER_TOOL_NAME, TurnInteractionMode, VolatileInjection,
    };
    use astra_turn_core::chat_history_openai::merge_skill_names_track;
    use astra_turn_core::chat_turn_edge_profile::{
        EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES, EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES,
        EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT, EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS,
        EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS, EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS,
    };

    #[test]
    fn typed_edge_skill_catalog_crosses_without_raw_listing_content() {
        let listing = json!({
            "role": "system",
            "content": "raw listing must stay client-local",
            "edge_skills": [{
                "name": "project-review",
                "description": "Review project changes",
                "aliases": ["review"]
            }]
        });
        let mut payload = json!({"edge_profile": {}});

        attach_typed_edge_skill_catalog(&mut payload, Some(&listing));

        assert_eq!(payload["edge_skills"][0]["name"], "project-review");
        assert!(
            payload.to_string().contains("project-review")
                && !payload
                    .to_string()
                    .contains("raw listing must stay client-local")
        );
    }

    #[test]
    fn server_loop_admission_excludes_client_owned_conversation_authority() {
        let prepared = json!({
            "messages": [{"role": "user", "content": "stale client history"}],
            "tool_results": [{"request_id": "call-1", "output": "already applied"}],
            "session_turn": 9,
            "turn_chain_id": "client-chain",
            "user_query_event_id": "client-event",
            "root_turn_journal_owned": true,
            "session_id": "session-1",
            "agent_id": "astra-cli",
            "user_intent": "implement the change",
            "model_selection": {"offering_id": "deepseek-flash"},
            "interaction_mode": "interactive",
            "edge_executor_id": "edge-1",
            "capabilities": ["bash"],
            "enabled_tools": ["web_fetch"],
            "edge_tools": [{"type": "function", "function": {"name": "bash"}}],
            "edge_profile": {"cwd": "/workspace"},
            "edge_skills": [{
                "name": "review-workspace",
                "description": "Review the current workspace",
                "aliases": ["review"]
            }],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
        });

        let admitted = server_loop_admission_payload(&prepared, "current request", true)
            .expect("Server loop admission");
        assert_eq!(admitted["message"], "current request");
        assert_eq!(
            admitted["context"]["edge_tools"][0]["function"]["name"],
            "bash"
        );
        assert_eq!(admitted["context"]["edge_profile"]["cwd"], "/workspace");
        assert_eq!(
            admitted["context"]["edge_skills"][0]["name"],
            "review-workspace"
        );
        assert_eq!(admitted["workspace_binding"]["kind"], "edge_workspace");
        assert_eq!(admitted["workspace_binding"]["root"], "/workspace");
        assert_eq!(admitted["workspace_binding"]["authority"], "read_write");
        assert_eq!(admitted["executor_binding"]["kind"], "edge_agent");
        assert_eq!(admitted["executor_binding"]["executor_id"], "edge-1");
        assert_eq!(admitted["executor_binding"]["transport"], "edge_ledger");
        assert_eq!(admitted["executor_binding"]["status"], "online");
        assert_eq!(admitted["enabled_tools"], json!(["web_fetch"]));
        assert_eq!(admitted["explain"], true);
        assert_eq!(admitted["execution_policy"]["turn_intent"], "auto");
        assert!(
            admitted.get("execution_time_budget").is_none(),
            "an absent wall budget must not fabricate execution authority"
        );
        for forbidden in [
            "messages",
            "tool_results",
            "session_turn",
            "turn_chain_id",
            "user_query_event_id",
            "root_turn_journal_owned",
            "conversation_authority",
        ] {
            assert!(
                admitted.get(forbidden).is_none(),
                "{forbidden} must remain Server-owned"
            );
        }
    }

    #[test]
    fn server_loop_admission_carries_typed_time_budget_outside_prompt_context() {
        let prepared = json!({
            "model_selection": {"offering_id": "generic-offering"},
            "edge_executor_id": "edge-1",
            "capabilities": [],
            "edge_profile": {
                "cwd": "/workspace",
                "system_prompt_override": "stable operator context"
            },
            "runtime_system_prompt": "stable runtime prompt"
        });

        let admitted = server_loop_admission_payload_with_execution_time_budget(
            &prepared,
            "request",
            false,
            Some(astra_services::runs::ExecutionTimeBudget {
                remaining_seconds: 37,
            }),
        )
        .expect("typed time budget admission");

        assert_eq!(admitted["execution_time_budget"]["remaining_seconds"], 37);
        assert_eq!(admitted["runtime_system_prompt"], "stable runtime prompt");
        assert!(
            admitted["context"]["edge_profile"]
                .get("execution_time_budget")
                .is_none(),
            "dynamic time must not enter the cache-stable edge profile"
        );
    }

    #[test]
    fn server_loop_admission_fails_closed_without_execution_binding() {
        let error = server_loop_admission_payload(
            &json!({
                "model_selection": {"offering_id": "deepseek-flash"},
                "capabilities": []
            }),
            "request",
            false,
        )
        .expect_err("missing edge executor must fail before transport");
        assert_eq!(
            error,
            "prepared developer loop payload has no executable edge identity"
        );
    }

    #[test]
    fn server_loop_admission_rejects_identity_without_workspace() {
        let error = server_loop_admission_payload(
            &json!({
                "model_selection": {"offering_id": "deepseek-flash"},
                "edge_executor_id": "edge-1",
                "capabilities": [],
                "edge_profile": {}
            }),
            "request",
            false,
        )
        .expect_err("an executor identity without an executable workspace must fail closed");

        assert_eq!(
            error,
            "prepared developer loop payload has no edge workspace root"
        );
    }

    #[test]
    fn server_loop_admission_never_widens_invalid_workspace_authority() {
        let error = server_loop_admission_payload(
            &json!({
                "model_selection": {"offering_id": "deepseek-flash"},
                "edge_executor_id": "edge-1",
                "capabilities": [],
                "edge_profile": {"cwd": "/workspace", "authority": "owner"}
            }),
            "request",
            false,
        )
        .expect_err("unknown authority must not silently become read-write");

        assert_eq!(
            error,
            "prepared developer loop payload has invalid workspace authority"
        );
    }
    use astra_turn_core::chat_turn_payload::attach_turn_identity;
    use serde_json::{Value, json};

    #[test]
    fn thinking_complexity_consumes_typed_llm_intent_without_text_matching() {
        let unjudged = thinking_complexity_signals("fix implement 修复 为什么", None);
        assert!(!unjudged.typed_lightweight);
        assert!(!unjudged.continues_current_objective);

        let quick_answer = TurnIntent::default()
            .with_requested_scenario(Scenario::QuickAnswer)
            .with_workspace_mutation(WorkspaceMutationIntent::ReadOnly);
        let judged = thinking_complexity_signals("arbitrary wording", Some(&quick_answer));
        assert!(judged.typed_lightweight);
        assert!(!judged.continues_current_objective);

        let continuation = quick_answer
            .clone()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Continue);
        let continued = thinking_complexity_signals("unrelated wording", Some(&continuation));
        assert!(continued.typed_lightweight);
        assert!(continued.continues_current_objective);

        let mutating = quick_answer.with_workspace_mutation(WorkspaceMutationIntent::MustMutate);
        assert!(
            !thinking_complexity_signals("why?", Some(&mutating)).typed_lightweight,
            "typed mutation intent must override a quick-answer scenario"
        );
    }

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

    fn schema_with_description(name: &str, description: &str) -> serde_json::Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    async fn prepare_payload_for_runtime_lane_test(
        runtime_required_texts: &[String],
        runtime_volatile_texts: &[String],
    ) -> Value {
        prepare_payload_with_messages_for_runtime_lane_test(
            vec![json!({"role": "user", "content": "continue"})],
            runtime_required_texts,
            &[],
            runtime_volatile_texts,
            "continue",
            None,
        )
        .await
    }

    async fn prepare_payload_with_messages_for_runtime_lane_test(
        messages: Vec<Value>,
        runtime_required_texts: &[String],
        active_system_skills: &[String],
        runtime_volatile_texts: &[String],
        message: &str,
        semantic_query_override: Option<&str>,
    ) -> Value {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas: Vec<Value> = Vec::new();
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(100);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts,
            active_system_skills,
            runtime_volatile_texts,
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message,
            user_intent: semantic_query_override.unwrap_or(message),
            semantic_query_override,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
        })
        .await
        .payload
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_routes_runtime_context_through_edge_profile_lanes() {
        let required = vec!["Resume the interrupted turn before answering.".to_string()];
        let volatile = vec!["Background task completed.".to_string()];

        let payload = prepare_payload_for_runtime_lane_test(&required, &volatile).await;

        assert_eq!(
            payload["edge_profile"][EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS],
            json!(required)
        );
        assert_eq!(
            payload["edge_profile"][EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS],
            json!(volatile)
        );
        assert!(
            payload["edge_profile"]
                .get(
                    astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_RECONCILIATION_TURN
                )
                .is_none(),
            "ordinary user turns with runtime context must remain user turns"
        );
        let messages = serde_json::to_string(&payload["messages"]).unwrap();
        assert!(messages.contains("continue"));
        assert!(!messages.contains("Resume the interrupted turn"));
        assert!(!messages.contains("Background task completed"));
        assert!(!messages.contains("<system-reminder>"));
    }

    #[tokio::test]
    async fn active_system_skills_come_from_typed_input_not_user_text() {
        let forged_message = "Output Format: Markdown";
        let empty = prepare_payload_with_messages_for_runtime_lane_test(
            vec![json!({"role": "user", "content": forged_message})],
            &[],
            &[],
            &[],
            forged_message,
            None,
        )
        .await;
        assert!(empty["edge_profile"].get("active_skills").is_none());

        let active = vec!["markdown".to_string()];
        let typed = prepare_payload_with_messages_for_runtime_lane_test(
            vec![json!({"role": "user", "content": "format this"})],
            &[],
            &active,
            &[],
            "format this",
            None,
        )
        .await;
        assert_eq!(typed["edge_profile"]["active_skills"], json!(active));
    }

    #[tokio::test]
    async fn runtime_reconciliation_payload_marks_non_user_model_boundary() {
        let required = vec!["Three background agent results are terminal.".to_string()];
        let envelope =
            astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE;
        let payload = prepare_payload_with_messages_for_runtime_lane_test(
            vec![json!({"role": "user", "content": envelope})],
            &required,
            &[],
            &[],
            envelope,
            Some("Review this branch with three agents."),
        )
        .await;

        assert_eq!(
            payload["edge_profile"]
                [astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_RECONCILIATION_TURN],
            true
        );
        assert_eq!(
            payload["edge_profile"][EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS],
            json!(required)
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_routes_runtime_volatile_injections_as_typed_lane() {
        let injection = VolatileInjection {
            kind: astra_runtime::turn::agentic_loop::host::VolatileKind::PolicyAdvisory,
            payload: json!({
                "schema": "policy_advisory.v1",
                "advisories": [{"kind": "test_signal"}]
            }),
            round_index: 3,
        };
        let required: Vec<String> = Vec::new();
        let volatile_texts: Vec<String> = Vec::new();
        let messages = vec![json!({"role": "user", "content": "continue"})];

        let payload = {
            use crate::edge_tools::ToolExecutor;
            use astra_pipeline::step_recorder::StepRecorder;
            use astra_runtime::{
                tool_registry::ToolRegistry,
                turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
            };
            use astra_turn_core::{
                interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard,
            };
            use std::{collections::HashSet, sync::Arc, time::Instant};

            let temp_dir = tempfile::tempdir().unwrap();
            let all_schemas: Vec<Value> = Vec::new();
            let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(100);
            let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
            let tool_results = Vec::new();
            let history: Vec<(String, String)> = Vec::new();
            let recent_tools: Vec<String> = Vec::new();
            let file_context: Vec<String> = Vec::new();
            let mut restricted_tools = HashSet::new();
            let mut valid_tool_names = HashSet::new();
            let mut widen_selection_pending = false;
            let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
            let turn_guard = TurnGuard::default();
            let mut turn_policy = TurnInteractionPolicy::default();
            let mut first_memoria_ms = None;
            let mut first_selection_report = None;
            let mut first_budget_pressure = 0.0;
            let mut first_context_assembly_ms = None;
            let mut all_selected_skills = Vec::new();

            prepare_chat_turn_payload(PrepareChatTurnRequest {
                messages: &messages,
                runtime_required_texts: &required,
                active_system_skills: &[],
                runtime_volatile_texts: &volatile_texts,
                runtime_volatile_injections: &[injection],
                ephemeral_prefix: None,
                current_session_id: Some("session-1"),
                offering_id: None,
                model: None,
                context_window_tokens: 200_000,
                effective_input_budget_tokens: 200_000,
                explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
                project_root: temp_dir.path(),
                message: "continue",
                user_intent: "continue",
                semantic_query_override: None,
                turn_intent: None,
                history: &history,
                recent_tools: &recent_tools,
                executor,
                registry: &registry,
                tool_results: &tool_results,
                all_schemas: &all_schemas,
                valid_tool_names: &mut valid_tool_names,
                turn_guard: &turn_guard,
                restricted_tools: &mut restricted_tools,
                widen_selection_pending: &mut widen_selection_pending,
                step_recorder: &mut step_recorder,
                file_context: &file_context,
                assembly_start: Instant::now(),
                telem: PrepareTurnTelemetry {
                    first_memoria_ms: &mut first_memoria_ms,
                    first_selection_report: &mut first_selection_report,
                    first_budget_pressure: &mut first_budget_pressure,
                    first_context_assembly_ms: &mut first_context_assembly_ms,
                    all_selected_skills: &mut all_selected_skills,
                    trace_collector: None,
                },
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
                lessons_text: None,
            })
            .await
        };

        let lane = &payload["edge_profile"][EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS];
        assert_eq!(lane[0]["kind"], json!("policy_advisory"));
        assert_eq!(lane[0]["delivery_class"], json!("decision_feedback"));
        assert_eq!(lane[0]["round_index"], json!(3));
        assert_eq!(lane[0]["payload"]["schema"], "policy_advisory.v1");
        assert_eq!(lane[0]["payload"]["advisories"][0]["kind"], "test_signal");
        assert!(
            payload["edge_profile"]
                .get(EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS)
                .is_none(),
            "internal runtime injections must not be flattened into the legacy text lane"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_normalizes_cli_server_prompt_boundary() {
        let messages = vec![
            json!({"role": "user", "content": "我说过的所有话"}),
            astra_turn_types::runtime_owned_message(
                "user",
                "Hydrated previous session context",
                astra_turn_types::RuntimeMessageDelivery::RequiredContext,
            ),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-analyze-session",
                    "type": "function",
                    "function": {"name": "skill", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "skill-auto-route-analyze-session", "content": "<skill-loaded name=\"analyze-session\"/>"}),
            json!({"role": "assistant", "content": "你问过我总结这段会话。"}),
            json!({"role": "user", "content": "继续"}),
        ];
        let structured = vec!["structured resume lane".to_string()];

        let payload = prepare_payload_with_messages_for_runtime_lane_test(
            messages,
            &structured,
            &[],
            &[],
            "continue",
            None,
        )
        .await;
        let payload_messages = serde_json::to_string(&payload["messages"]).unwrap();

        assert!(payload_messages.contains("我说过的所有话"));
        assert!(payload_messages.contains("你问过我总结这段会话。"));
        assert!(payload_messages.contains("继续"));
        assert!(!payload_messages.contains("skill-auto-route"));
        assert!(!payload_messages.contains("<skill-loaded"));
        assert_eq!(
            payload["edge_profile"][EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS],
            json!([
                "Hydrated previous session context",
                "structured resume lane"
            ])
        );
    }

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into()]);
        assert_eq!(v, vec!["a", "b"]);
    }

    #[test]
    fn inject_runtime_turn_overrides_adds_plan_fields() {
        let mut payload = json!({});
        inject_runtime_turn_overrides(
            &mut payload,
            true,
            Some("sub-1"),
            Some("high"),
            Some("coder"),
        );

        assert!(payload.get("skill_search").is_none());
        assert_eq!(payload["is_plan_subtask"], json!(true));
        assert_eq!(payload["rollback_on_failure"], json!(true));
        assert_eq!(payload["rollback_boundary"], json!("turn"));
        assert_eq!(payload["plan_subtask_id"], json!("sub-1"));
        assert_eq!(payload["effort"], json!("high"));
        assert_eq!(payload["agent_type"], json!("coder"));
    }

    #[test]
    fn shared_turn_provenance_identity_adds_authoritative_ids() {
        let mut payload = json!({});
        assert!(attach_turn_identity(
            &mut payload,
            2,
            Some("root-chain"),
            Some("root-query")
        ));
        assert_eq!(payload["session_turn"], json!(2));
        assert_eq!(payload["turn_chain_id"], json!("root-chain"));
        assert_eq!(payload["user_query_event_id"], json!("root-query"));
    }

    #[test]
    fn chat_turn_budget_pressure_uses_effective_input_budget_for_large_context_models() {
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn_with_context_window,
        };

        let mut registry = ToolRegistry::new(Vec::new());
        let large_description = "x".repeat(360_000);
        registry.inject_schema_always_load(
            schema_with_description("large_always_load", &large_description),
            true,
        );
        let messages: Vec<Value> = Vec::new();
        let schema_tokens = registry.total_always_load_token_cost() as usize;

        let effective_budget_pressure = chat_turn_budget_pressure(&messages, &registry, 100_000);
        let raw_context_window_pressure =
            budget_pressure_for_chat_turn_with_context_window(&messages, schema_tokens, 800_000);

        assert!(
            effective_budget_pressure >= 0.6,
            "100K effective input budget should see {schema_tokens} schema tokens as compact pressure, got {effective_budget_pressure}"
        );
        assert!(
            raw_context_window_pressure < 0.3,
            "raw 800K context-window pressure would hide the same schema load, got {raw_context_window_pressure}"
        );
        assert!(
            effective_budget_pressure > raw_context_window_pressure,
            "turn preparation must be governed by effective input budget, not raw context window"
        );
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
        assert_eq!(
            turns[0].content_preview,
            "user: first · assistant: reply one"
        );
        assert_eq!(turns[1].turn_index, 1);
        assert_eq!(turns[1].role, "assistant");
        assert!(turns[1].has_tool_calls);
        assert_eq!(
            turns[1].content_preview,
            "user: second · assistant: calling tool · tool: tool output · assistant: final answer"
        );
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
    fn cross_session_memory_projection_preserves_hit_metadata_without_side_effects() {
        let hits = vec![
            astra_tools::memoria::BoostSearchHit {
                memory_id: Some("mem-1".to_string()),
                content: "runtime retrieval should use structured memory decisions".to_string(),
                score: Some(0.92),
            },
            astra_tools::memoria::BoostSearchHit {
                memory_id: None,
                content: "other memory content".to_string(),
                score: Some(0.74),
            },
        ];

        let projection = project_cross_session_memory_hits("runtime retrieval", &hits);

        assert_eq!(projection.contents.len(), 2);
        assert_eq!(projection.feedback_ids, vec!["mem-1"]);
        assert!(!projection.ranked.is_empty());
        assert!(
            projection
                .ranked
                .iter()
                .any(|(content, _)| content.contains("structured memory decisions"))
        );
    }

    #[test]
    fn cross_session_memory_projection_handles_empty_hits() {
        let projection = project_cross_session_memory_hits("runtime retrieval", &[]);

        assert!(projection.contents.is_empty());
        assert!(projection.ranked.is_empty());
        assert!(projection.preferred_repos.is_empty());
        assert!(projection.feedback_ids.is_empty());
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
        assert_eq!(policy.observation_tool_names, vec!["mo_query".to_string()]);
        assert!(policy.allow_ask_user);
    }

    #[test]
    fn turn_policy_from_payload_edge_tools_defaults_empty_when_missing() {
        let policy =
            super::turn_policy_from_payload_edge_tools(&json!({}), TurnInteractionMode::Auto);

        assert!(policy.visible_tool_names.is_empty());
        assert!(policy.observation_tool_names.is_empty());
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

    /// Regression: the final ToolSelectionReport's `schema_budget_used` must be derived
    /// entirely from the explicit `schema_budget_used` argument, NOT from any stale
    /// `schema_budget_used` field on a pre-existing report. This contract is what
    /// permits removing stale intermediate recomputation at the call site (the value
    /// set on `surface_report.schema_budget_used` was overwritten by
    /// the final visible-schema token total and never consumed).
    #[test]
    fn surface_report_from_visible_schemas_is_single_source_for_budget() {
        let schemas = vec![schema("grep"), schema("read_file")];

        let report = super::surface_report_from_visible_schemas(
            &schemas, 42,  // schema_budget_used — arbitrary, must pass through verbatim
            100, // schema_budget_total
        );

        assert_eq!(
            report.visible_tools,
            vec!["grep".to_string(), "read_file".to_string()]
        );
        assert_eq!(report.visible_count, 2);
        assert_eq!(report.schema_budget_used, 42);
        assert_eq!(report.schema_budget_total, 100);
    }

    #[test]
    fn first_class_browser_surface_is_structural_and_idempotent() {
        let all_schemas = vec![schema("web_fetch"), schema("web_search"), schema("bash")];
        let mut selected = vec![schema("bash")];
        let mut report = empty_report(200);

        super::inject_first_class_browser_tools(&mut selected, &mut report, &all_schemas);
        super::inject_first_class_browser_tools(&mut selected, &mut report, &all_schemas);

        let names: Vec<_> = selected
            .iter()
            .filter_map(super::tool_schema_name)
            .collect();
        assert_eq!(names, vec!["bash", "web_fetch", "web_search"]);
        assert_eq!(report.visible_tools, vec!["web_fetch", "web_search"]);
        assert_eq!(report.visible_count, 2);
    }

    #[test]
    fn first_class_browser_surface_does_not_invent_unavailable_tools() {
        let all_schemas = vec![schema("bash")];
        let mut selected = Vec::new();
        let mut report = empty_report(200);

        super::inject_first_class_browser_tools(&mut selected, &mut report, &all_schemas);

        assert!(selected.is_empty());
        assert!(report.visible_tools.is_empty());
        assert_eq!(report.visible_count, 0);
    }

    // ── Tool surface decision: structural signals, not text-based ────────
    //
    // The decision is driven by the tool pipeline state (visible schemas,
    // surface report, context signals) — never by NLP inference on the
    // user message text. This keeps the tool surface deterministic and
    // prompt-cache-friendly.

    /// Helper: empty report with the given schema_budget_total.
    fn empty_report(
        schema_budget_total: u32,
    ) -> astra_turn_core::tool_registry_report::ToolSelectionReport {
        astra_turn_core::tool_registry_report::ToolSelectionReport {
            visible_tools: Vec::new(),
            visible_count: 0,
            schema_budget_used: 0,
            schema_budget_total,
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
            "schema_budget_total == 0 with no prior candidates → structurally starved"
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
            report: astra_turn_core::tool_registry_report::ToolSelectionReport,
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
        let count_only = astra_turn_core::tool_registry_report::ToolSelectionReport {
            visible_count: 3,
            schema_budget_total: 100,
            ..empty_report(100)
        };
        assert_eq!(
            super::tool_surface_should_inject(&[], &count_only, false, false, false, false),
            (true, "surface_report_names"),
        );

        // schema_budget_total == 0 but HadToolsBeforeRuntimeFilter is already set →
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
        use astra_turn_core::tool_registry_report::ToolSelectionReport;
        use astra_turn_core::tool_schema_prune::inject_skill_allowed_tools;

        let all_schemas = [
            schema("bash"),
            schema("read_file"),
            schema("grep"),
            schema("glob"),
        ];

        // Surface included bash and read_file, but not grep/glob
        let mut turn_schemas = vec![schema("bash"), schema("read_file")];
        let mut report = ToolSelectionReport {
            visible_tools: vec!["bash".into(), "read_file".into()],
            visible_count: 2,
            schema_budget_used: 0,
            schema_budget_total: 0,
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

    #[test]
    fn enabled_optional_tools_are_derived_from_executable_schema_facts() {
        let schemas = astra_tools::schemas::all_tool_schemas();
        let enabled = super::enabled_optional_tool_names_from_schemas(schemas.iter());

        assert!(enabled.contains(&"web_fetch".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"web_search".to_string()), "{enabled:?}");
        assert!(!enabled.contains(&"read_file".to_string()), "{enabled:?}");
        assert!(!enabled.contains(&"bash".to_string()), "{enabled:?}");
    }

    #[test]
    fn enabled_optional_tools_do_not_reintroduce_absent_schemas() {
        let filtered_surface = [schema("read_file"), schema("web_fetch")];
        let enabled = super::enabled_optional_tool_names_from_schemas(filtered_surface.iter());

        assert_eq!(enabled, vec!["web_fetch"]);
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(100);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "inspect the repo state"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: Some("offer-qwen"),
            model: Some("qwen3.7-max"),
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repo state",
            user_intent: "inspect the repo state",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
        })
        .await;

        let edge_tool_names: Vec<&str> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert_eq!(payload["model_selection"]["offering_id"], "offer-qwen");
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
            first_selection_report
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
            first_selection_report
                .as_ref()
                .map(|report| report.schema_budget_used),
            Some(expected_visible_schema_tokens),
            "final surface telemetry schema_budget_used must use the same full visible-tool surface as visible_count"
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
    async fn prepare_chat_turn_payload_keeps_plan_tools_when_plan_mode_inactive() {
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
        // Budget of 2 would normally expose only the 2 most relevant tools.
        // Plan-control tools are injected regardless of active mode so the
        // schema surface stays stable across plan/default transitions.
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "inspect the repo state"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repo state",
            user_intent: "inspect the repo state",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
            first_selection_report
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
    async fn prepare_chat_turn_payload_honors_typed_non_work_act_and_preserves_activation() {
        use crate::edge_tools::ToolExecutor;
        use astra_config::user_profile::{TurnCommunicativeAct, TurnIntent};
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
        let social_intent =
            TurnIntent::default().with_communicative_act(TurnCommunicativeAct::Social);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let empty_surface_message = "empty tool surface";
        let messages = vec![json!({"role": "user", "content": empty_surface_message})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder =
            StepRecorder::new("test-user", "session-empty-selector", "task-empty-selector");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-empty-surface"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: empty_surface_message,
            user_intent: empty_surface_message,
            semantic_query_override: None,
            turn_intent: Some(&social_intent),
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
        })
        .await;

        let edge_tools = payload["edge_tools"].as_array().unwrap();
        assert!(
            edge_tools.is_empty(),
            "a typed social turn should not include tool schemas: {:?}",
            edge_tools
                .iter()
                .filter_map(|schema| schema["function"]["name"].as_str())
                .collect::<Vec<_>>()
        );
        let enabled_tools = payload["enabled_tools"]
            .as_array()
            .expect("CLI payload must publish its executable optional capability set");
        assert!(
            enabled_tools.iter().any(|name| name == "web_fetch"),
            "deferred optional capabilities are request facts even when this turn is tool-free: {enabled_tools:?}"
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
            first_selection_report
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
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new(
            "test-user",
            "session-pending-activation",
            "task-pending-activation",
        );
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-pending-activation"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: empty_surface_message,
            user_intent: empty_surface_message,
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
            "payload assembly must preserve retained deferred materialization"
        );
        let _ = executor.execute("memory", &json!({})).await;
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "a successful call must not revoke retained schema materialization"
        );
        executor.clear_current_tool_surface_for_tests();

        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(0);
        let messages = vec![json!({"role": "user", "content": "inspect the repository"})];
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-empty", "task-empty");
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-empty"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "inspect the repository",
            user_intent: "inspect the repository",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: Arc::clone(&executor),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
                .is_some_and(|text| text.contains("<deferred-tools>")),
            "tool_search visibility must be paired with a deferred manifest"
        );
    }

    #[tokio::test]
    async fn prepare_chat_turn_payload_does_not_equate_bash_with_background_task_state() {
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
        let executor = Arc::new(
            ToolExecutor::new(temp_dir.path())
                .with_bg_task_commands(Arc::new(std::sync::Mutex::new(Vec::new()))),
        );
        let messages = vec![json!({"role": "user", "content": "run make check"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "run make check",
            user_intent: "run make check",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
                !edge_tool_names.contains(&name),
                "Selecting bash must not advertise inactive background control {name}: {edge_tool_names:?}"
            );
        }
    }

    #[test]
    fn runtime_filter_removes_background_controls_without_a_registry_binding() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut schemas = vec![
            schema("bash"),
            schema("task_output"),
            schema("task_list"),
            schema("task_stop"),
        ];
        let mut report = surface_report_from_visible_schemas(&schemas, 4, 4);
        let executor = crate::edge_tools::ToolExecutor::new(temp_dir.path());

        assert!(runtime_filter_turn_schemas_and_report(
            &executor,
            &mut schemas,
            &mut report,
        ));
        assert_eq!(
            astra_turn_core::tool::schema::tool_names_from_schemas(&schemas),
            std::collections::HashSet::from(["bash".to_string()])
        );
        assert_eq!(report.visible_tools, vec!["bash".to_string()]);
        assert_eq!(report.visible_count, 1);
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let message = "修复 timeout handling";
        let user_intent = "review timeout handling and propose a fix";
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
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report =
            Some(astra_turn_core::tool_registry_report::ToolSelectionReport {
                visible_tools: vec!["stale_first_round_tool".to_string()],
                visible_count: 1,
                schema_budget_used: 1,
                schema_budget_total: 2,
            });
        let mut first_budget_pressure = 0.99;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();
        let trace_collector = TurnTraceCollector::new("turn-1", "session-1");
        let semantic_query_override =
            "Task: review timeout handling\nAssistant summary: Need a fix.\nFollow-up: 修复?";

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message,
            user_intent,
            semantic_query_override: Some(semantic_query_override),
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: Some(&trace_collector),
            },
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
            lessons_text: None,
        })
        .await;

        let trace = trace_collector.finalize();
        assert_eq!(
            payload["user_intent"].as_str(),
            Some(user_intent),
            "edge payload must preserve structured user_intent separately from prompt-facing message"
        );
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
        let plan_event = step_recorder
            .events()
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.event_type,
                    astra_pipeline::step_protocol::StepEventType::StepStarted
                )
            })
            .expect("round plan event");
        assert_eq!(
            plan_event.payload.as_ref().unwrap()["visible_tools"],
            json!(edge_tool_names),
            "round-level step telemetry must describe the current final tool surface, not the immutable first-round report"
        );
        assert_eq!(
            first_selection_report
                .as_ref()
                .map(|report| report.visible_tools.as_slice()),
            Some(["stale_first_round_tool".to_string()].as_slice()),
            "turn-level first-surface telemetry must remain immutable"
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
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
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "implement the approved plan",
            user_intent: "implement the approved plan",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
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
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "remember this",
            user_intent: "remember this",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
        let expected_pinned_tokens: u64 = edge_tool_names
            .iter()
            .map(|name| u64::from(registry.token_cost(name)))
            .sum();
        assert_eq!(
            payload.pinned_tool_schema_tokens, expected_pinned_tokens,
            "next-round compaction must account for the exact deferred schema materialized in this payload"
        );
        assert!(
            valid_tool_names.contains("memory"),
            "activated deferred tool must be admitted only after it is injected"
        );
        assert_eq!(valid_tool_names, edge_tool_names);
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "payload assembly must preserve retained deferred materialization"
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
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
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "no deferred tools",
            user_intent: "no deferred tools",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));

        let messages =
            vec![json!({"role": "user", "content": "delegate review with parallel agents"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "delegate review with parallel agents",
            user_intent: "delegate review with parallel agents",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(1);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        executor.debug_stage_pending_round_tool_boost_for_test(&["agent_fanout"]);

        let messages = vec![json!({"role": "user", "content": "fan out this work"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "fan out this work",
            user_intent: "fan out this work",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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
    async fn prepare_chat_turn_payload_consumes_widen_selection_pending_once() {
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
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(2);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "update the file"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = true;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let mut turn_guard = TurnGuard::default();
        turn_guard.health.record_failure("write_file");
        turn_guard.health.record_failure("write_file");
        turn_guard.health.record_failure("write_file");
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let first_payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "update the file",
            user_intent: "update the file",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor: executor.clone(),
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
        })
        .await;

        let first_tool_names: Vec<&str> = first_payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        assert!(first_tool_names.contains(&"write_file"));
        assert!(!widen_selection_pending);

        let second_payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "update the file",
            user_intent: "update the file",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
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

    #[tokio::test]
    async fn prepare_chat_turn_payload_excludes_legacy_task_context() {
        use crate::edge_tools::ToolExecutor;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_runtime::{
            tool_registry::ToolRegistry,
            turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
        };
        use astra_turn_core::{interaction_types::TurnInteractionPolicy, turn_guard::TurnGuard};
        use std::{collections::HashSet, sync::Arc, time::Instant};

        let temp_dir = tempfile::tempdir().unwrap();
        let all_schemas = vec![schema("bash")];
        let registry = ToolRegistry::new(all_schemas.clone()).with_schema_budget(100);
        let executor = Arc::new(ToolExecutor::new(temp_dir.path()));
        let messages = vec![json!({"role": "user", "content": "fix the bug"})];
        let tool_results = Vec::new();
        let history: Vec<(String, String)> = Vec::new();
        let recent_tools: Vec<String> = Vec::new();
        let file_context: Vec<String> = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut valid_tool_names = HashSet::new();
        let mut widen_selection_pending = false;
        let mut step_recorder = StepRecorder::new("test-user", "session-1", "task-1");
        let turn_guard = TurnGuard::default();
        let mut turn_policy = TurnInteractionPolicy::default();
        let mut first_memoria_ms = None;
        let mut first_selection_report = None;
        let mut first_budget_pressure = 0.0;
        let mut first_context_assembly_ms = None;
        let mut all_selected_skills = Vec::new();

        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            runtime_required_texts: &[],
            active_system_skills: &[],
            runtime_volatile_texts: &[],
            runtime_volatile_injections: &[],
            ephemeral_prefix: None,
            current_session_id: Some("session-1"),
            offering_id: None,
            model: None,
            context_window_tokens: 200_000,
            effective_input_budget_tokens: 200_000,
            explain: AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off),
            project_root: temp_dir.path(),
            message: "fix the bug",
            user_intent: "fix the bug",
            semantic_query_override: None,
            turn_intent: None,
            history: &history,
            recent_tools: &recent_tools,
            executor,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            valid_tool_names: &mut valid_tool_names,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            widen_selection_pending: &mut widen_selection_pending,
            step_recorder: &mut step_recorder,
            file_context: &file_context,
            assembly_start: Instant::now(),
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
                trace_collector: None,
            },
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
            lessons_text: None,
        })
        .await;

        assert!(payload["edge_profile"].get("task_context_text").is_none());
    }
}

// Note: Environment variable parsing tests for `chat_turn_timing_stderr_enabled` were removed
// because unsafe `std::env::set_var` is unsound in multi-threaded programs. The function's
// logic is trivial (pattern matching on env var values) and not worth testing with unsafe code.
