use std::future::Future;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::*;

/// Plan-only turns can sit quiet for a long time (payload assembly, HTTP, queue, TTFR).
/// Show one in-place stderr line (`PlanAssembleLineSpinner`) instead of spamming new lines.
///
/// When `MO_AGENT_CHAT_TURN_TIMING` / `MO_DEBUG` emit `[chat-turn timing]` lines to stderr, the
/// spinner is disabled — those lines use newlines and would fight the `\r` status line.
///
/// `plan_line_release`: when `Some`, shows `PlanAssembleLineSpinner` and passes the same `Arc` to
/// `/chat/turn` fetch so the line is cleared before SSE (`Waiting for model` + reasoning preview).
/// The flag uses `Release` (after successful POST) / `Acquire` (spinner poll) — enough for this
/// boolean handoff; no payload is communicated through the atomic.
async fn plan_only_llm_heartbeat_wrap<F, T>(
    assemble_elapsed_origin: Option<Instant>,
    plan_line_release: Option<Arc<AtomicBool>>,
    inner: F,
) -> T
where
    F: Future<Output = T>,
{
    if plan_line_release.is_none() {
        return inner.await;
    }
    let origin = assemble_elapsed_origin.unwrap_or_else(Instant::now);
    let spinner = crate::stream_render::PlanAssembleLineSpinner::start_with_origin_release(
        origin,
        plan_line_release.clone(),
    );
    let out = inner.await;
    spinner.stop_clear();
    out
}

pub(super) struct ReplTurnContext<'a> {
    pub(super) api: &'a mo_thin_client::ThinClient,
    pub(super) profile: Option<&'a str>,
    pub(super) selector: &'a dyn tool_selector::ToolSelector,
}

/// Enqueue a journal event for async cloud ingestion (if matrix runtime is available).
fn enqueue_ingestion(state: &ReplState, event: &session_journal::JournalEvent) {
    let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
    if let Some(mc) = state.matrix_runtime.as_ref() {
        mc.enqueue_journal_events(user_id, event);
    }
}

/// Public wrapper for enqueue_ingestion — used by main.rs for session_end.
pub(super) fn enqueue_ingestion_pub(state: &ReplState, event: &session_journal::JournalEvent) {
    enqueue_ingestion(state, event);
}

/// Pull a few Memoria hits after compact so the shortened context keeps **session-relevant**
/// recall (similar in spirit to Claude Code keeping session memory as an anchor).
const COMPACT_ANCHOR_QUERY_MAX: usize = 220;
const COMPACT_ANCHOR_TOP_K: u32 = 5;
const COMPACT_ANCHOR_MAX_LINES: usize = 4;
const COMPACT_ANCHOR_LINE_MAX: usize = 140;
const COMPACT_ANCHOR_TOTAL_MAX: usize = 700;

pub(super) async fn fetch_compact_memory_anchor_snippet(
    api: &mo_thin_client::ThinClient,
    token: &str,
    session_id: Option<&str>,
    summary_seed: &str,
) -> Option<String> {
    let seed: String = summary_seed
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(summary_seed)
        .chars()
        .take(COMPACT_ANCHOR_QUERY_MAX)
        .collect();
    let seed = seed.trim();
    if seed.is_empty() {
        return None;
    }
    let mut q = String::new();
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        q.push_str(sid);
        q.push(' ');
    }
    q.push_str(seed);
    let payload = serde_json::json!({
        "query": q,
        "top_k": COMPACT_ANCHOR_TOP_K,
    });
    let resp = api.post_memory_search_json(token, &payload).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let arr: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
    if arr.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for m in arr.iter().take(COMPACT_ANCHOR_MAX_LINES) {
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let line = if let Some(entry) = prompts::memory_proto::MemoryEntry::parse(content) {
            entry.display_line()
        } else {
            let mtype = m
                .get("memory_type")
                .and_then(|v| v.as_str())
                .unwrap_or("note");
            let preview: String = content.chars().take(100).collect();
            format!("[{mtype}] {preview}")
        };
        let line: String = line.chars().take(COMPACT_ANCHOR_LINE_MAX).collect();
        if line.trim().is_empty() {
            continue;
        }
        let next_len = total + line.len() + 1;
        if next_len > COMPACT_ANCHOR_TOTAL_MAX {
            break;
        }
        total = next_len;
        lines.push(format!("- {line}"));
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

pub(super) fn compact_assistant_message(
    trimmed: usize,
    summary: &str,
    anchor: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(a) = anchor.filter(|s| !s.trim().is_empty()) {
        out.push_str("[Session memory anchor]\n");
        out.push_str(a.trim());
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "[Prior context — {trimmed} turns compacted]\n\n{summary}"
    ));
    out
}

enum TurnAttempt {
    Completed(Box<Result<StreamResult, String>>),
    Interrupted,
}

pub(super) async fn handle_chat_input(
    line: String,
    current_token: Option<&str>,
    state: &mut ReplState,
    ctx: ReplTurnContext<'_>,
) -> Result<(), String> {
    let token = match current_token {
        Some(token) => token,
        None => {
            eprintln!(
                "{}",
                "  Not logged in. Use /login to authenticate.".yellow()
            );
            return Ok(());
        }
    };

    eprintln!();

    let effective_line = build_effective_line(&line, state);
    let turn_start = Instant::now();

    maybe_auto_compact(state, &ctx, token).await?;

    let session_id = state.session_id.clone();
    match run_chat_turn(state, &ctx, token, &effective_line, session_id.as_deref()).await {
        TurnAttempt::Interrupted => {
            state.last_turn_interrupted = true;
            return Ok(());
        }
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => {
                state.last_turn_interrupted = false;
                apply_turn_success(state, ctx.selector, ctx.profile, &line, result, turn_start);
                return Ok(());
            }
            Err(error) => {
                if is_session_not_found_error(&error) && state.session_id.is_some() {
                    let _ = clear_profile_last_session(ctx.profile);
                    state.session_id = None;
                    eprintln!(
                        "{}",
                        "  Session not found. Creating a new session…".yellow()
                    );

                    match run_chat_turn(state, &ctx, token, &effective_line, None).await {
                        TurnAttempt::Interrupted => {
                            state.last_turn_interrupted = true;
                            return Ok(());
                        }
                        TurnAttempt::Completed(result) => match *result {
                            Ok(result) => {
                                apply_turn_success(
                                    state,
                                    ctx.selector,
                                    ctx.profile,
                                    &line,
                                    result,
                                    turn_start,
                                );
                                return Ok(());
                            }
                            Err(retry_error) => {
                                report_turn_error(state, &line, &retry_error, turn_start);
                                return Ok(());
                            }
                        },
                    }
                }

                report_turn_error(state, &line, &error, turn_start);
            }
        },
    }

    Ok(())
}

pub(super) fn build_effective_line(line: &str, state: &ReplState) -> String {
    let mut effective_line = if let (Some(skill_name), Some(source)) = (
        state.skill_dev_name.as_deref(),
        state.skill_dev_context.as_deref(),
    ) {
        format!(
            "{}{line}",
            prompts::build_skill_dev_prefix(skill_name, source)
        )
    } else {
        line.to_string()
    };

    if !state.active_system_skills.is_empty() {
        let skill_block = prompts::build_skill_instructions(&state.active_system_skills);
        effective_line = format!("{skill_block}\n\n{effective_line}");
    }

    effective_line
}

async fn maybe_auto_compact(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
) -> Result<(), String> {
    if state.history.len() <= state.context_budget.keep_recent_turns {
        return Ok(());
    }

    let est_messages = history_as_messages(&state.history);
    let est_tokens = prompts::estimate_tokens(&est_messages);
    if !state.context_budget.should_compact(est_tokens) {
        return Ok(());
    }

    eprintln!(
        "  {}",
        format!(
            "⟳ Auto-compacting ({} turns, ~{}k tokens > {}k limit)…",
            state.history.len(),
            est_tokens / 1000,
            state.context_budget.compact_trigger() / 1000,
        )
        .dim()
    );

    let mut auto_pm_compact =
        PermissionManager::with_project(true, &std::env::current_dir().unwrap_or_default());
    let compact_result = stream_chat_sse(ChatTurnParams {
        api: ctx.api,
        token,
        message: prompts::COMPACT_SUMMARY_REQUEST,
        session_id: state.session_id.as_deref(),
        model: state.model.as_deref(),
        explain: ExplainMode::Off,
        render_md: false,
        history: &state.history,
        perm_manager: &mut auto_pm_compact,
        verbose_mode: false,
        quiet: true,
        suppress_intermediate_output: false,
        selector: ctx.selector,
        recent_tools: &[],
        tool_health_entries: &[],
        skill_registry: &state.skill_registry,
        plan_only_chat: false,
        hide_streaming_assistant_text: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
    })
    .await;

    if let Ok(result) = compact_result {
        apply_auto_compact_result(state, ctx, token, result).await?;
    }

    Ok(())
}

pub(super) fn history_as_messages(history: &[(String, String)]) -> Vec<serde_json::Value> {
    history
        .iter()
        .flat_map(|(user, assistant)| {
            let mut pair = Vec::with_capacity(2);
            if !user.is_empty() {
                pair.push(serde_json::json!({"role":"user","content":user}));
            }
            if !assistant.is_empty() {
                pair.push(serde_json::json!({"role":"assistant","content":assistant}));
            }
            pair
        })
        .collect()
}

async fn apply_auto_compact_result(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    result: StreamResult,
) -> Result<(), String> {
    let summary = result.full_text.trim().to_string();
    if summary.is_empty() {
        return Ok(());
    }

    let keep = state.context_budget.keep_recent_turns;
    let total = state.history.len();
    let trimmed = total.saturating_sub(keep);
    if trimmed == 0 {
        return Ok(());
    }

    let anchor =
        fetch_compact_memory_anchor_snippet(ctx.api, token, state.session_id.as_deref(), &summary)
            .await;
    let assistant_text = compact_assistant_message(trimmed, &summary, anchor.as_deref());
    let summary_entry = (String::new(), assistant_text);
    let mut new_history = vec![summary_entry];
    new_history.extend_from_slice(&state.history[trimmed..]);
    state.history = new_history;
    state.recent_tools.clear();

    let entry = prompts::memory_proto::MemoryEntry::new(
        prompts::memory_proto::NS_EPISODE,
        prompts::memory_proto::ST_AUTO,
        &summary,
    );
    let meta = prompts::memory_proto::EntryMeta::from_session(
        state.session_id.as_deref(),
        state.turn,
        prompts::memory_proto::SRC_AUTO_COMPACT,
    );
    if let Err(e) = ctx
        .api
        .post_memory_store_json(token, &entry.to_store_payload_with_meta(&meta))
        .await
    {
        mo_agent_core::agent_warn!("repl_turn", "failed to persist compacted memory: {e}");
    }

    eprintln!(
        "  {}",
        format!(
            "✓ Compacted {trimmed} turns → {} in context",
            state.history.len()
        )
        .green()
    );
    if state.plan_mode.is_some() || state.executing_plan.is_some() {
        eprintln!(
            "{}",
            "  Tip: Plan context was shortened — if steps feel stale, refresh `/plan`.".dim()
        );
    }

    Ok(())
}

/// Outcome of a single-shot plan LLM call (`plan_only_chat` + same REPL history as normal chat).
pub(super) enum PlanOnlyLlmOutcome {
    Ok(Box<StreamResult>),
    Err(String),
    Interrupted,
}

/// Same post-turn accounting as normal chat (`apply_turn_success`), except the caller supplies
/// the `user_line` shown in journal/history (e.g. user goal vs internal decomposition prompt).
pub(super) fn apply_plan_only_stream_artifacts(
    state: &mut ReplState,
    profile: Option<&str>,
    selector: &dyn tool_selector::ToolSelector,
    result: &StreamResult,
    user_line: &str,
    turn_start: Instant,
) {
    if let Some(session_id) = result.session_id.as_deref() {
        persist_last_session_id(profile, session_id);
        initialize_journal(state, session_id);
        state.session_id = Some(session_id.to_string());
        state.run_id = result.run_id.clone();
    }

    state.turn += 1;
    state.total_prompt_tokens += result.prompt_tokens;
    state.total_completion_tokens += result.completion_tokens;
    state.last_response = Some(result.full_text.clone());
    state
        .history
        .push((user_line.to_string(), result.full_text.clone()));
    state.recent_tools = result.tools_used.clone();

    if !result.tool_health_export.is_empty() {
        state.tool_health_entries = result.tool_health_export.clone();
    }

    commit_turn_journal_workspace_and_sidecars(state, user_line, result, turn_start);
    record_selector_turn_outcome(state, selector, user_line, result);

    if result.tool_calls_count == 0
        && looks_like_live_query_with_context(user_line, &state.recent_tools)
    {
        eprintln!(
            "{}",
            "  ⚠ Warning: This answer was generated without tool calls. Data may be hallucinated."
                .yellow()
        );
    }
}

/// Plan-mode LLM turn: same `/chat/turn` + SSE path as normal chat (`consume_turn_sse`), including
/// `openai_messages_from_repl_history(history, message)` with the current REPL history snapshot,
/// plus `plan_only_chat` (no write tools), cancellation, usage, and reasoning/thinking handling.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_plan_only_llm_turn(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    message: &str,
    session_id: Option<&str>,
    suppress_intermediate_output: bool,
    render_md: bool,
    quiet: bool,
) -> PlanOnlyLlmOutcome {
    let history_snapshot = state.history.clone();
    let cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    let cancel_token_for_signal = cancel_token.clone();

    let assemble_elapsed_origin = state.plan_mode.as_mut().map(|pm| {
        *pm.assemble_wall_start
            .get_or_insert_with(std::time::Instant::now)
    });

    let stderr_timing = crate::chat_stream::chat_turn_timing_stderr_enabled();
    let enable_plan_line = !quiet
        && !suppress_intermediate_output
        && std::io::stderr().is_terminal()
        && !stderr_timing;
    let plan_line_release = enable_plan_line.then(|| Arc::new(AtomicBool::new(false)));

    plan_only_llm_heartbeat_wrap(assemble_elapsed_origin, plan_line_release.clone(), async {
        tokio::select! {
            result = stream_chat_sse(ChatTurnParams {
                api: ctx.api,
                token,
                message,
                session_id,
                model: state.model.as_deref(),
                explain: state.explain,
                render_md,
                history: &history_snapshot,
                perm_manager: &mut state.perm_manager,
                verbose_mode: state.verbose_mode,
                quiet,
                suppress_intermediate_output,
                selector: ctx.selector,
                recent_tools: &state.recent_tools,
                tool_health_entries: &state.tool_health_entries,
                skill_registry: &state.skill_registry,
                plan_only_chat: true,
                hide_streaming_assistant_text: true,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: state.delegation_engine.clone(),
                cancel_token: Some(cancel_token),
                plan_assemble_line_release: plan_line_release.clone(),
            }) => match result {
                Ok(r) => PlanOnlyLlmOutcome::Ok(Box::new(r)),
                Err(e) => PlanOnlyLlmOutcome::Err(e),
            },
            _ = tokio::signal::ctrl_c() => {
                cancel_token_for_signal.cancel();
                eprintln!("\n{}", "  Interrupted.".dim());
                PlanOnlyLlmOutcome::Interrupted
            }
        }
    })
    .await
}

async fn run_chat_turn(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    message: &str,
    session_id: Option<&str>,
) -> TurnAttempt {
    // NOTE: Skill selection is now done by LLM during tool selection.
    // The skill_registry is passed to stream_chat_sse for loading instructions
    // when the LLM selects a skill.

    // Create a cancellation token that can interrupt SSE streaming mid-flight.
    let cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    let cancel_token_for_signal = cancel_token.clone();

    tokio::select! {
        result = stream_chat_sse(ChatTurnParams {
            api: ctx.api,
            token,
            message,
            session_id,
            model: state.model.as_deref(),
            explain: state.explain,
            render_md: true,
            history: &state.history,
            perm_manager: &mut state.perm_manager,
            verbose_mode: state.verbose_mode,
            quiet: false,
            suppress_intermediate_output: false,
            selector: ctx.selector,
            recent_tools: &state.recent_tools,
            tool_health_entries: &state.tool_health_entries,
            skill_registry: &state.skill_registry,
            plan_only_chat: state.chat_plan_only && state.current_plan_subtask_id.is_none(),
            hide_streaming_assistant_text: false,
            is_plan_subtask: state.current_plan_subtask_id.is_some(),
            plan_subtask_id: state.current_plan_subtask_id.as_deref(),
            delegation_engine: state.delegation_engine.clone(),
            cancel_token: Some(cancel_token),
            plan_assemble_line_release: None,
        }) => TurnAttempt::Completed(Box::new(result)),
        _ = tokio::signal::ctrl_c() => {
            // Trigger cancellation to interrupt any in-flight SSE streaming.
            cancel_token_for_signal.cancel();
            eprintln!("\n{}", "  Interrupted.".dim());
            TurnAttempt::Interrupted
        }
    }
}

/// After `state.turn` has been incremented: journal turn row, workspace + checkpoints,
/// stall/verdict/step sidecars. Shared by normal chat and plan-only LLM turns.
fn commit_turn_journal_workspace_and_sidecars(
    state: &mut ReplState,
    line: &str,
    result: &StreamResult,
    turn_start: Instant,
) {
    if let Some(journal) = state.journal.as_ref() {
        let turn_event = session_journal::JournalEvent::turn(
            state.session_id.as_deref(),
            state.turn,
            state.model.as_deref(),
            line,
            &result.full_text,
            result.tool_calls_count,
            result.prompt_tokens,
            result.completion_tokens,
            turn_start.elapsed().as_millis() as u64,
        )
        .with_tool_selection(
            result.tools_selected.clone(),
            result.selected_skills.clone(),
            result.tools_used.clone(),
            result.budget_used,
        )
        .with_tool_calls(result.tool_call_records.clone())
        .with_budget_pressure(result.budget_pressure)
        .with_plan_subtask(state.current_plan_subtask_id.as_deref())
        .with_ttft(result.ttft_ms)
        .with_context_time(result.context_ms)
        .with_selector_strategy(result.selector_strategy.clone())
        .with_selector_time(result.selector_ms)
        .with_selector_tokens(result.selector_tokens_in, result.selector_tokens_out)
        .with_memoria_time(result.memoria_ms);

        // Store for /turn command
        state.last_turn_event = Some(turn_event.clone());

        if let Err(e) = journal.append(&turn_event) {
            mo_agent_core::agent_warn!("journal", "failed to write turn event: {e}");
        }
        enqueue_ingestion(state, &turn_event);

        // Update workspace metadata per-turn
        if let Some(sid) = state.session_id.as_deref()
            && let Ok(mut ws) = mo_agent_services::session_workspace::read_workspace(sid)
        {
            ws.record_turn(result.prompt_tokens, result.completion_tokens);

            // Persist plan state to workspace for session resume
            ws.executing_plan_json = state
                .executing_plan
                .as_ref()
                .and_then(|p| serde_json::to_string(p).ok());
            ws.plan_goal = state.executing_plan_goal.clone();
            ws.plan_config_json = state
                .plan_execution_config
                .as_ref()
                .and_then(|c| serde_json::to_string(c).ok());
            ws.plan_execution_rounds = state.plan_execution_rounds;

            // Persist durable task contract for session resume
            ws.contract_json = state
                .durable_task_state
                .as_ref()
                .and_then(|d| serde_json::to_string(&d.contract).ok());

            // Persist operator corrections so they survive a crash mid-plan
            ws.plan_corrections = state.plan_execution_corrections.clone();

            // Check if checkpoint is due
            if mo_agent_services::session_checkpoint::should_checkpoint(
                ws.turn_count,
                mo_agent_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ) {
                ws.record_checkpoint();
                let cp = mo_agent_services::session_checkpoint::Checkpoint {
                    number: ws.checkpoints.len() as u32,
                    turn: ws.turn_count,
                    title: format!("Turn {} checkpoint", ws.turn_count),
                    summary: format!(
                        "Accumulated {} tokens ({} in, {} out). Tools: {}",
                        ws.total_tokens_in + ws.total_tokens_out,
                        ws.total_tokens_in,
                        ws.total_tokens_out,
                        result.tools_used.join(", "),
                    ),
                    tools_used: result.tools_used.clone(),
                    total_tokens: ws.total_tokens_in + ws.total_tokens_out,
                    had_stalls: false,
                    error_count: 0,
                    contract_state_json: state
                        .durable_task_state
                        .as_ref()
                        .and_then(|d| serde_json::to_string(&d.contract).ok()),
                };
                let _ = mo_agent_services::session_checkpoint::write_checkpoint(sid, &cp);

                // Push checkpoint to MatrixOne for cross-device availability
                if let Some(ref mc) = state.matrix_runtime {
                    let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
                    let pool = mc.shared_pool().get().clone();
                    let sid_owned = sid.to_string();
                    let user_id_owned = user_id.to_string();
                    let cp_clone = cp.clone();
                    tokio::spawn(async move {
                        let _ = mo_agent_services::session_restore::push_checkpoint_to_cloud(
                            &pool,
                            &sid_owned,
                            &user_id_owned,
                            &cp_clone,
                        )
                        .await;
                    });
                }
                let cp_event = session_journal::JournalEvent::checkpoint(
                    Some(sid),
                    ws.turn_count,
                    &cp.summary,
                    cp.total_tokens,
                    cp.tools_used.len(),
                );
                let _ = journal.append(&cp_event);
                enqueue_ingestion(state, &cp_event);
            }

            // Push Step Protocol heavy checkpoint to MatrixOne (full state for recovery)
            if let Some(ref mc) = state.matrix_runtime
                && let Some(ref step_cp) = result.last_heavy_checkpoint
                && let Ok(state_json) = serde_json::to_string(step_cp)
            {
                let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
                let pool = mc.shared_pool().get().clone();
                let sid_owned = sid.to_string();
                let user_id_owned = user_id.to_string();
                let cp_number = result
                    .step_recorder_summary
                    .as_ref()
                    .map(|s| s.checkpoints)
                    .unwrap_or(0);
                // Extract metadata from the checkpoint for column storage
                let (tier, turn, title, tools_json): (String, u32, String, String) = match step_cp {
                    mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Light(l) => (
                        "light".to_string(),
                        0u32,
                        format!("step:{}", l.step_id),
                        "[]".to_string(),
                    ),
                    mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(h) => {
                        let tools = serde_json::to_string(&h.recent_tools)
                            .unwrap_or_else(|_| "[]".to_string());
                        (
                            "heavy".to_string(),
                            0u32,
                            format!("step:{}", h.light.step_id),
                            tools,
                        )
                    }
                };
                tokio::spawn(async move {
                    let _ = mo_agent_services::session_restore::push_step_checkpoint_to_cloud(
                        &pool,
                        &sid_owned,
                        &user_id_owned,
                        cp_number,
                        turn,
                        &tier,
                        &title,
                        &tools_json,
                        &state_json,
                    )
                    .await;
                });
            }

            // Push plan state to cloud at checkpoint boundaries
            if let Some(ref mc) = state.matrix_runtime
                && mo_agent_services::session_checkpoint::should_checkpoint(
                    ws.turn_count,
                    mo_agent_services::session_checkpoint::CHECKPOINT_INTERVAL,
                )
            {
                let pool = mc.shared_pool().get().clone();
                let sid_owned = sid.to_string();
                let plan_json = ws.executing_plan_json.clone();
                let goal = ws.plan_goal.clone();
                let config = ws.plan_config_json.clone();
                let rounds = ws.plan_execution_rounds;
                tokio::spawn(async move {
                    let _ = mo_agent_services::session_restore::push_plan_state_to_cloud(
                        &pool,
                        &sid_owned,
                        plan_json.as_deref(),
                        goal.as_deref(),
                        config.as_deref(),
                        rounds,
                    )
                    .await;
                });
            }

            let _ = mo_agent_services::session_workspace::write_workspace(&ws);
        }

        // Log stall events to journal (use state.turn for user turn, not internal loop turn)
        for (stall_type, _) in &result.stall_events {
            let stall_event = session_journal::JournalEvent::stall_detected(
                state.session_id.as_deref(),
                state.turn,
                stall_type,
                0, // nudge_count not tracked per-event; stall_type conveys severity
                0.0,
                &[],
            );
            let _ = journal.append(&stall_event);
            enqueue_ingestion(state, &stall_event);
        }

        // Log TurnGuard verdict events to journal (use state.turn for user turn)
        for ve in &result.verdict_events {
            let verdict_event = session_journal::JournalEvent::turn_guard_verdict(
                state.session_id.as_deref(),
                state.turn,
                &ve.severity,
                &ve.injections,
                &ve.avoid_tools,
                ve.force_stop,
                ve.nudge_count,
                ve.total_errors,
                ve.deprioritized_count,
                ve.total_timeouts,
                ve.total_cache_hits,
                ve.flaky_count,
            );
            let _ = journal.append(&verdict_event);
            enqueue_ingestion(state, &verdict_event);
        }

        // Log Step Protocol recorder summary (audit trail for execution phases)
        if let Some(ref summary) = result.step_recorder_summary {
            let summary_text = format!(
                "step_recorder: turns={} tools={} phases={} time={}ms",
                summary.turns,
                summary.total_tools,
                summary.phase_log.len(),
                summary.total_time_ms,
            );
            let recorder_event = session_journal::JournalEvent::checkpoint(
                state.session_id.as_deref(),
                state.turn,
                &summary_text,
                result.prompt_tokens + result.completion_tokens,
                result.tool_calls_count as usize,
            );
            let _ = journal.append(&recorder_event);
            enqueue_ingestion(state, &recorder_event);
        }
    }
}

fn record_selector_turn_outcome(
    state: &mut ReplState,
    selector: &dyn tool_selector::ToolSelector,
    line: &str,
    result: &StreamResult,
) {
    use mo_agent_runtime::pipeline::evaluation::{ToolCallInfo, evaluate_turn};
    use mo_agent_runtime::pipeline::routing::RoutingEngine;
    let routing = RoutingEngine::analyze(line, state.turn, &state.recent_tools, &[], vec![]);
    let is_live_query = looks_like_live_query_with_context(line, &state.recent_tools);

    let tool_infos: Vec<ToolCallInfo> = result
        .tool_call_records
        .iter()
        .map(|r| ToolCallInfo {
            name: r.name.clone(),
            ok: r.ok,
            ms: r.ms,
            error: r.error.clone(),
            output_bytes: r.output_bytes,
        })
        .collect();

    let has_verdict_warning = result
        .verdict_events
        .iter()
        .any(|v| v.severity == "Warning" || v.severity == "Critical");

    let eval = evaluate_turn(
        &tool_infos,
        result.stall_events.len(),
        has_verdict_warning,
        result.budget_pressure,
        is_live_query,
    );

    selector.record_outcome(
        line,
        &result.tools_used,
        routing.task_type,
        routing.domain_hint,
        eval.success,
        eval.quality,
        false,
        None,
    );
}

fn apply_turn_success(
    state: &mut ReplState,
    selector: &dyn tool_selector::ToolSelector,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    if let Some(session_id) = result.session_id.as_deref() {
        persist_last_session_id(profile, session_id);
        initialize_journal(state, session_id);
        state.session_id = Some(session_id.to_string());
        state.run_id = result.run_id.clone();
    }

    state.turn += 1;
    state.total_prompt_tokens += result.prompt_tokens;
    state.total_completion_tokens += result.completion_tokens;
    state.last_response = Some(result.full_text.clone());
    state
        .history
        .push((line.to_string(), result.full_text.clone()));
    state.recent_tools = result.tools_used.clone();

    // Persist tool health for cross-session error budgets
    if !result.tool_health_export.is_empty() {
        state.tool_health_entries = result.tool_health_export.clone();
    }

    commit_turn_journal_workspace_and_sidecars(state, line, &result, turn_start);
    record_selector_turn_outcome(state, selector, line, &result);

    if result.tool_calls_count == 0 && looks_like_live_query_with_context(line, &state.recent_tools)
    {
        eprintln!(
            "{}",
            "  ⚠ Warning: This answer was generated without tool calls. Data may be hallucinated."
                .yellow()
        );
    }
}

pub(super) fn initialize_journal_pub(state: &mut ReplState, session_id: &str) {
    initialize_journal(state, session_id);
}

pub(super) fn persist_last_session_id(profile: Option<&str>, session_id: &str) {
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    let entry = creds.profiles.entry(name).or_default();
    entry.last_session_id = Some(session_id.to_string());
    let _ = save_credentials(&creds);
}

fn initialize_journal(state: &mut ReplState, session_id: &str) {
    if state.journal.is_some() {
        return;
    }

    state.journal = session_journal::JournalWriter::new(session_id).ok();
    if let Some(journal) = state.journal.as_ref() {
        let start_event =
            session_journal::JournalEvent::session_start(Some(session_id), state.model.as_deref());
        let _ = journal.append(&start_event);
        // enqueue_ingestion skips if matrix_runtime is None
        enqueue_ingestion(state, &start_event);
    }

    // Initialize workspace metadata alongside journal
    let ws = mo_agent_services::session_workspace::WorkspaceMetadata::new(
        session_id,
        state.model.as_deref().unwrap_or("default"),
    );
    let _ = mo_agent_services::session_workspace::write_workspace(&ws);
}

fn report_turn_error(state: &ReplState, line: &str, error: &str, turn_start: Instant) {
    let lower = error.to_lowercase();
    if error.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("could not validate credentials")
    {
        eprintln!("{}", "  Session expired. Run /login to refresh.".yellow());
    } else {
        eprintln!("{}", format!("  ✗  {error}").red());
    }

    if let Some(journal) = state.journal.as_ref() {
        let err_event = session_journal::JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            state.model.as_deref(),
            line,
            error,
            turn_start.elapsed().as_millis() as u64,
        );
        let _ = journal.append(&err_event);
        enqueue_ingestion(state, &err_event);
    }
}
