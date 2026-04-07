use std::time::Instant;

use super::*;

pub(super) struct ReplTurnContext<'a> {
    pub(super) api: &'a astra_thin_client::ThinClient,
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
    api: &astra_thin_client::ThinClient,
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
    Completed(Box<Result<StreamResult, crate::TurnFailure>>),
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
            if let Some(journal) = state.journal.as_ref() {
                let evt = session_journal::JournalEvent::turn_error(
                    state.session_id.as_deref(),
                    state.turn + 1,
                    state.model.as_deref(),
                    &line,
                    "user_interrupted (Ctrl+C)",
                    turn_start.elapsed().as_millis() as u64,
                );
                let _ = journal.append(&evt);
                enqueue_ingestion(state, &evt);
            }
            return Ok(());
        }
        TurnAttempt::Completed(result) => match *result {
            Ok(result) => {
                state.last_turn_interrupted = false;
                apply_turn_success(state, ctx.selector, ctx.profile, &line, result, turn_start);
                return Ok(());
            }
            Err(failure) => {
                if is_session_not_found_error(&failure.error) && state.session_id.is_some() {
                    let _ = clear_profile_last_session(ctx.profile);
                    state.session_id = None;
                    eprintln!(
                        "{}",
                        "  Session not found. Creating a new session…".yellow()
                    );

                    match run_chat_turn(state, &ctx, token, &effective_line, None).await {
                        TurnAttempt::Interrupted => {
                            state.last_turn_interrupted = true;
                            if let Some(journal) = state.journal.as_ref() {
                                let evt = session_journal::JournalEvent::turn_error(
                                    state.session_id.as_deref(),
                                    state.turn + 1,
                                    state.model.as_deref(),
                                    &line,
                                    "user_interrupted (Ctrl+C)",
                                    turn_start.elapsed().as_millis() as u64,
                                );
                                let _ = journal.append(&evt);
                                enqueue_ingestion(state, &evt);
                            }
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
                            Err(retry_failure) => {
                                report_turn_failure(state, ctx.profile, &line, &retry_failure, turn_start);
                                return Ok(());
                            }
                        },
                    }
                }

                report_turn_failure(state, ctx.profile, &line, &failure, turn_start);
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

    if let Some(anchor) = state
        .continuation_anchor
        .as_deref()
        .filter(|_| is_short_continuation_prompt(line))
    {
        effective_line = format!(
            "[Continuation anchor]\nResume the active task/thread below unless the user explicitly changes topic.\n{anchor}\n\n[User follow-up]\n{effective_line}"
        );
    }

    effective_line
}

fn is_short_continuation_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 16 {
        return false;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "continue" | "continue." | "continue!" | "go on" | "resume"
    ) || matches!(trimmed, "继续" | "继续。" | "继续！")
}

fn build_continuation_anchor(
    state: &ReplState,
    line: &str,
    result: &StreamResult,
) -> Option<String> {
    let user_line = line.trim();
    if user_line.is_empty() {
        return state.continuation_anchor.clone();
    }

    let assistant_summary = result
        .full_text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or_default();

    let assistant_summary: String = assistant_summary.chars().take(220).collect();
    let user_summary: String = user_line.chars().take(220).collect();

    if assistant_summary.is_empty() {
        return Some(format!("Latest user task: {user_summary}"));
    }

    Some(format!(
        "Latest user task: {user_summary}\nLatest assistant direction: {assistant_summary}"
    ))
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
        unified_skill_registry: &state.unified_skill_registry,
        plan_only_chat: false,
        hide_streaming_assistant_text: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: Some(state.mcp_manager.clone()),
        skill_search: &state.skill_search,
        skill_quality_tracker: &mut state.skill_quality_tracker,
        discovered_skills: None,
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
        astra_core::agent_warn!("repl_turn", "failed to persist compacted memory: {e}");
    }

    eprintln!(
        "  {}",
        format!(
            "{} Compacted {trimmed} turns → {} in context",
            crate::theme::icon_ok(),
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

async fn run_chat_turn(
    state: &mut ReplState,
    ctx: &ReplTurnContext<'_>,
    token: &str,
    message: &str,
    session_id: Option<&str>,
) -> TurnAttempt {
    // Skill selection is handled by the `skill` tool in the agentic loop.
    // The unified_skill_registry provides all skill resolution.

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
            unified_skill_registry: &state.unified_skill_registry,
            plan_only_chat: state.chat_plan_only && state.current_plan_subtask_id.is_none(),
            hide_streaming_assistant_text: false,
            is_plan_subtask: state.current_plan_subtask_id.is_some(),
            plan_subtask_id: state.current_plan_subtask_id.as_deref(),
            delegation_engine: state.delegation_engine.clone(),
            cancel_token: Some(cancel_token),
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: Some(state.mcp_manager.clone()),
            skill_search: &state.skill_search,
                skill_quality_tracker: &mut state.skill_quality_tracker,
                discovered_skills: Some(&mut state.discovered_skills),
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
        .with_memoria_time(result.memoria_ms)
        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);

        // Store for /turn command
        state.last_turn_event = Some(turn_event.clone());

        if let Err(e) = journal.append(&turn_event) {
            astra_core::agent_warn!("journal", "failed to write turn event: {e}");
        }
        enqueue_ingestion(state, &turn_event);

        // Update workspace metadata per-turn
        if let Some(sid) = state.session_id.as_deref()
            && let Ok(mut ws) = astra_services::session_workspace::read_workspace(sid)
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
            if astra_services::session_checkpoint::should_checkpoint(
                ws.turn_count,
                astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ) {
                ws.record_checkpoint();
                let cp = astra_services::session_checkpoint::Checkpoint {
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
                let _ = astra_services::session_checkpoint::write_checkpoint(sid, &cp);

                // Push checkpoint to MatrixOne for cross-device availability
                if let Some(ref mc) = state.matrix_runtime {
                    let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
                    let pool = mc.shared_pool().get().clone();
                    let sid_owned = sid.to_string();
                    let user_id_owned = user_id.to_string();
                    let cp_clone = cp.clone();
                    tokio::spawn(async move {
                        let _ = astra_services::session_restore::push_checkpoint_to_cloud(
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
                    astra_runtime::pipeline::step_protocol::StepCheckpoint::Light(l) => (
                        "light".to_string(),
                        0u32,
                        format!("step:{}", l.step_id),
                        "[]".to_string(),
                    ),
                    astra_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(h) => {
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
                    let _ = astra_services::session_restore::push_step_checkpoint_to_cloud(
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
                && astra_services::session_checkpoint::should_checkpoint(
                    ws.turn_count,
                    astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
                )
            {
                let pool = mc.shared_pool().get().clone();
                let sid_owned = sid.to_string();
                let plan_json = ws.executing_plan_json.clone();
                let goal = ws.plan_goal.clone();
                let config = ws.plan_config_json.clone();
                let rounds = ws.plan_execution_rounds;
                tokio::spawn(async move {
                    let _ = astra_services::session_restore::push_plan_state_to_cloud(
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

            let _ = astra_services::session_workspace::write_workspace(&ws);
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
    use astra_runtime::pipeline::evaluation::{ToolCallInfo, evaluate_turn};
    use astra_runtime::pipeline::routing::RoutingEngine;
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
    state.total_cache_read_tokens += result.cache_read_tokens;
    state.total_cache_creation_tokens += result.cache_creation_tokens;

    // Accumulate per-turn cost
    let turn_cost = crate::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );
    state.total_session_cost += turn_cost;
    state.last_response = Some(result.full_text.clone());
    state.continuation_anchor = build_continuation_anchor(state, line, &result);
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

    // ── Skill auto-improvement check ─────────────────────────────────────
    check_skill_improvement(state, line, &result);

    // ── Post-turn status line ────────────────────────────────────────────
    print_turn_status_line(state, &result, turn_start);

    if result.tool_calls_count == 0 && looks_like_live_query_with_context(line, &state.recent_tools)
    {
        eprintln!(
            "{}",
            "  ⚠ Warning: This answer was generated without tool calls. Data may be hallucinated."
                .yellow()
        );
    }
}

fn print_turn_status_line(state: &ReplState, result: &StreamResult, turn_start: Instant) {
    let elapsed = turn_start.elapsed();
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };

    let total_tokens = result.prompt_tokens + result.completion_tokens;
    let tokens_str = if total_tokens > 1000 {
        format!("{:.1}k", total_tokens as f64 / 1000.0)
    } else {
        format!("{total_tokens}")
    };
    let prompt_short = if result.prompt_tokens > 1000 {
        format!("{:.1}k", result.prompt_tokens as f64 / 1000.0)
    } else {
        format!("{}", result.prompt_tokens)
    };
    let completion_short = if result.completion_tokens > 1000 {
        format!("{:.1}k", result.completion_tokens as f64 / 1000.0)
    } else {
        format!("{}", result.completion_tokens)
    };

    // Per-turn cost
    let turn_cost = crate::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );

    let mut parts = Vec::new();

    if let Some(ref model) = state.model {
        parts.push(format!("model:{model}"));
    }

    parts.push(format!(
        "tokens:{tokens_str} (↑{prompt_short} ↓{completion_short})"
    ));

    // Show turn cost (skip if pricing not available)
    if turn_cost > 0.0 {
        parts.push(crate::format_cost(turn_cost));
    }

    parts.push(elapsed_str);

    if result.tool_calls_count > 0 {
        parts.push(format!(
            "{} tool{}",
            result.tool_calls_count,
            if result.tool_calls_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }

    // Cache savings indicator
    if result.cache_read_tokens > 0 {
        let cache_pct =
            result.cache_read_tokens as f64 / (result.prompt_tokens + result.cache_read_tokens).max(1) as f64 * 100.0;
        parts.push(format!("cache:{cache_pct:.0}%"));
    }

    let line = format!("  ─ {} ─", parts.join(" │ "));
    eprintln!("{}", line.dim());

    // Session total on second line (only after first turn with pricing)
    let session_cost = state.total_session_cost + turn_cost;
    if session_cost > 0.0 && state.turn > 0 {
        let session_line = format!("  session: {}", crate::format_cost(session_cost));
        eprintln!("{}", session_line.dim());
    }

    let w = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80);
    let rule = "─".repeat(w.min(72));
    eprintln!("{}", rule.dim());
}

/// Check if the skill improvement tracker should trigger analysis.
///
/// After every N user turns (TURN_BATCH_SIZE), checks whether the recent conversation
/// contains corrections or improvements for any active filesystem skill.
fn check_skill_improvement(state: &mut ReplState, _line: &str, _result: &StreamResult) {
    if !state.skill_improvement_tracker.should_analyze(state.turn) {
        return;
    }

    // Find active filesystem skills (only .astra/skills/ are improvable)
    let registry = state.unified_skill_registry.clone();
    let manifests = registry.all_manifests();
    let filesystem_skills: Vec<_> = manifests
        .iter()
        .filter(|m| {
            matches!(
                m.source,
                astra_runtime::skills::manifest::SkillSourceKind::Local
            )
        })
        .collect();

    if filesystem_skills.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    // Build recent messages for analysis
    let recent: Vec<astra_runtime::skills::improvement::RecentMessage> = state
        .history
        .iter()
        .rev()
        .take(astra_runtime::skills::improvement::TURN_BATCH_SIZE as usize)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                astra_runtime::skills::improvement::RecentMessage {
                    role: "user".into(),
                    content: user.clone(),
                },
                astra_runtime::skills::improvement::RecentMessage {
                    role: "assistant".into(),
                    content: assistant.clone(),
                },
            ]
        })
        .collect();

    if recent.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    // Log that analysis is due — actual LLM analysis deferred to future iteration.
    // The prompt builders (build_analysis_prompt, build_rewrite_prompt) are ready
    // in astra_runtime::skills::improvement, but calling LLM from here requires
    // async context + API key plumbing that's better handled via a dedicated
    // background task or post-turn hook.
    astra_core::agent_info!(
        "skill",
        "improvement check: {} filesystem skill(s) eligible, {} recent messages — analysis ready",
        filesystem_skills.len(),
        recent.len(),
    );

    state.skill_improvement_tracker.mark_analyzed(state.turn);
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
    let ws = astra_services::session_workspace::WorkspaceMetadata::new(
        session_id,
        state.model.as_deref().unwrap_or("default"),
    );
    let _ = astra_services::session_workspace::write_workspace(&ws);
}

/// Report a turn failure with enriched partial data from the agentic loop.
fn report_turn_failure(
    state: &mut ReplState,
    profile: Option<&str>,
    line: &str,
    failure: &crate::TurnFailure,
    turn_start: Instant,
) {
    let lower = failure.error.to_lowercase();
    if failure.error.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("could not validate credentials")
    {
        eprintln!("{}", "  Session expired. Run /login to refresh.".yellow());
    } else {
        eprintln!(
            "  {} {}",
            crate::theme::icon_err(),
            failure.error.as_str().red()
        );
    }

    // If the turn carried a session_id but the journal was never initialised
    // (first turn failed before apply_turn_success), bootstrap it now so the
    // error is persisted and visible via /turn and /debug.
    if state.journal.is_none() {
        if let Some(sid) = failure.partial.session_id.as_deref().filter(|s| !s.is_empty()) {
            initialize_journal(state, sid);
            persist_last_session_id(profile, sid);
            state.session_id = Some(sid.to_string());
        }
    }

    if let Some(journal) = state.journal.as_ref() {
        let mut err_event = session_journal::JournalEvent::turn_error(
            state.session_id.as_deref(),
            state.turn + 1,
            state.model.as_deref(),
            line,
            &failure.error,
            turn_start.elapsed().as_millis() as u64,
        );

        // Enrich with partial data rescued from AgenticLoopState
        if !failure.partial.tool_call_records.is_empty() {
            err_event.tool_calls = Some(failure.partial.tool_call_records.clone());
        }
        if failure.partial.prompt_tokens > 0 {
            err_event.tokens_in = Some(failure.partial.prompt_tokens);
        }
        if failure.partial.completion_tokens > 0 {
            err_event.tokens_out = Some(failure.partial.completion_tokens);
        }
        if failure.partial.tool_calls_count > 0 {
            err_event.tool_count = Some(failure.partial.tool_calls_count);
        }
        if !failure.partial.tools_used.is_empty() {
            err_event.tools_used = Some(failure.partial.tools_used.clone());
        }
        if !failure.partial.stall_events.is_empty() || !failure.partial.verdict_events.is_empty() {
            err_event.metadata = Some(serde_json::json!({
                "error_type": "turn_failure",
                "stall_count": failure.partial.stall_events.len(),
                "verdict_count": failure.partial.verdict_events.len(),
                "has_checkpoint": failure.partial.last_heavy_checkpoint.is_some(),
            }));
        }

        let _ = journal.append(&err_event);
        enqueue_ingestion(state, &err_event);
        state.last_turn_event = Some(err_event);
    }

    // Preserve partial text in conversation history so the next turn has
    // context about what the model already did before the interruption.
    if !failure.partial.partial_text.is_empty() {
        let partial_with_note = format!(
            "[Interrupted: {}]\n\n{}",
            failure.error, failure.partial.partial_text
        );
        state
            .history
            .push((line.to_string(), partial_with_note));
        state.last_response = Some(failure.partial.partial_text.clone());
    }
}

/// Copy plan / durable-task fields from REPL into workspace before checkpointing.
fn sync_plan_fields_to_workspace(
    state: &ReplState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
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
    ws.contract_json = state
        .durable_task_state
        .as_ref()
        .and_then(|d| serde_json::to_string(&d.contract).ok());
    ws.plan_corrections = state.plan_execution_corrections.clone();
}

/// Next numeric id for `step_checkpoints/<NNNNNN>-*.json`.
fn next_step_checkpoint_number(sid: &str) -> Result<u32, String> {
    let existing = astra_runtime::pipeline::step_checkpoint::list_checkpoints(sid)
        .map_err(|e| format!("list step checkpoints: {e}"))?;
    Ok(existing
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// Build heavy step checkpoint from current REPL history (OpenAI-style messages).
fn build_manual_heavy_step_checkpoint(
    state: &ReplState,
    sid: &str,
) -> astra_runtime::pipeline::step_protocol::StepCheckpoint {
    use astra_runtime::pipeline::step_protocol::{
        ExecutionCursor, HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint,
        epoch_ms,
    };

    let mut messages = Vec::new();
    for (u, a) in &state.history {
        messages.push(serde_json::json!({ "role": "user", "content": u }));
        messages.push(serde_json::json!({ "role": "assistant", "content": a }));
    }

    let max_turns = std::env::var("MO_MAX_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50u32);
    let now_ms = epoch_ms();
    let total_tok = state.total_prompt_tokens + state.total_completion_tokens;

    let light = LightCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        cursor: ExecutionCursor::default(),
        step_id: "repl-manual".to_string(),
        task_id: state.run_id.clone().unwrap_or_else(|| "repl".to_string()),
        agent_id: sid.to_string(),
        progress: 1.0,
        total_tokens: total_tok,
        created_at: now_ms,
    };
    let heavy = HeavyCheckpoint {
        light,
        messages,
        budget_remaining_tokens: 0,
        budget_remaining_rounds: max_turns.saturating_sub(state.turn),
        blocked_tools: Vec::new(),
        recent_tools: state.recent_tools.clone(),
        learning_snapshot_id: None,
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: Vec::new(),
    };
    StepCheckpoint::Heavy(Box::new(heavy))
}

/// Persist heavy JSON + composite snapshot index. Run **before** mutating workspace checkpoint list
/// so a failure here does not leave `workspace.yaml` ahead of disk recovery files.
fn persist_manual_heavy_and_composite(
    sid: &str,
    turn: u32,
    title: &str,
    next_step: u32,
    step_cp: &astra_runtime::pipeline::step_protocol::StepCheckpoint,
) -> Result<std::path::PathBuf, String> {
    use astra_runtime::pipeline::step_checkpoint::{
        read_composite_snapshot_index, write_composite_snapshot_index, write_step_checkpoint,
    };

    let heavy_path = write_step_checkpoint(sid, next_step, step_cp)
        .map_err(|e| format!("write heavy step checkpoint: {e}"))?;

    let snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.to_string(), turn)
            .label(format!("manual:{title}"))
            .session_state(format!("{next_step:06}-heavy.json"))
            .workspace_state(sid.to_string())
            .build();
    let mut index = read_composite_snapshot_index(sid).unwrap_or_default();
    index.snapshots.push(snapshot);
    if let Err(e) = write_composite_snapshot_index(sid, &index) {
        return Err(format!(
            "write composite snapshot index: {e} (heavy file already at {})",
            heavy_path.display()
        ));
    }

    Ok(heavy_path)
}

/// After heavy JSON exists: bump workspace checkpoint list, write markdown, journal, ingestion, `workspace.yaml`.
fn persist_manual_session_checkpoint_layer(
    state: &ReplState,
    journal: &session_journal::JournalWriter,
    sid: &str,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
    title: &str,
) -> Result<
    (
        std::path::PathBuf,
        u32,
        astra_services::session_checkpoint::Checkpoint,
    ),
    String,
> {
    ws.record_checkpoint();
    let cp_number = ws.checkpoints.len() as u32;
    let summary = format!(
        "User /checkpoint at turn {} — {} ({} turns in history, {} recent tools).",
        ws.turn_count,
        title,
        state.history.len(),
        state.recent_tools.len(),
    );

    let cp = astra_services::session_checkpoint::Checkpoint {
        number: cp_number,
        turn: ws.turn_count,
        title: title.to_string(),
        summary: summary.clone(),
        tools_used: state.recent_tools.clone(),
        total_tokens: ws.total_tokens_in + ws.total_tokens_out,
        had_stalls: false,
        error_count: 0,
        contract_state_json: state
            .durable_task_state
            .as_ref()
            .and_then(|d| serde_json::to_string(&d.contract).ok()),
    };

    let cp_path = astra_services::session_checkpoint::write_checkpoint(sid, &cp)
        .map_err(|e| format!("write session checkpoint: {e}"))?;

    let cp_event = session_journal::JournalEvent::checkpoint(
        Some(sid),
        ws.turn_count,
        &summary,
        ws.total_tokens_in + ws.total_tokens_out,
        state.recent_tools.len(),
    );
    if let Err(e) = journal.append(&cp_event) {
        astra_core::agent_warn!(
            "checkpoint",
            "journal append failed after writing session checkpoint markdown (file={}): {e}",
            cp_path.display()
        );
        return Err(format!(
            "journal append failed (checkpoint markdown exists at {}): {e}",
            cp_path.display()
        ));
    }
    enqueue_ingestion(state, &cp_event);

    astra_services::session_workspace::write_workspace(ws)
        .map_err(|e| format!("write workspace: {e}"))?;

    Ok((cp_path, cp_number, cp))
}

/// Queue session + step checkpoint uploads (best-effort; errors only in logs).
fn spawn_manual_checkpoint_cloud_uploads(
    state: &ReplState,
    sid: &str,
    session_cp: &astra_services::session_checkpoint::Checkpoint,
    next_step: u32,
    turn: u32,
    title: &str,
    step_cp: &astra_runtime::pipeline::step_protocol::StepCheckpoint,
) {
    let Some(ref mc) = state.matrix_runtime else {
        return;
    };
    let user_id = state
        .ingestion_user_id
        .as_deref()
        .unwrap_or("anonymous")
        .to_string();
    let user_id_step = user_id.clone();
    let pool = mc.shared_pool().get().clone();
    let sid_owned = sid.to_string();
    let cp_clone = session_cp.clone();
    tokio::spawn(async move {
        if let Err(e) = astra_services::session_restore::push_checkpoint_to_cloud(
            &pool, &sid_owned, &user_id, &cp_clone,
        )
        .await
        {
            astra_core::agent_warn!("checkpoint", "cloud push session checkpoint: {e}");
        }
    });

    let pool2 = mc.shared_pool().get().clone();
    let sid_step = sid.to_string();
    let title_owned = title.to_string();
    let state_json = serde_json::to_string(step_cp).unwrap_or_default();
    let tools_json =
        serde_json::to_string(&state.recent_tools).unwrap_or_else(|_| "[]".to_string());
    tokio::spawn(async move {
        if let Err(e) = astra_services::session_restore::push_step_checkpoint_to_cloud(
            &pool2,
            &sid_step,
            &user_id_step,
            next_step,
            turn,
            "heavy",
            &title_owned,
            &tools_json,
            &state_json,
        )
        .await
        {
            astra_core::agent_warn!("checkpoint", "cloud push step checkpoint: {e}");
        }
    });
}

/// User-initiated checkpoint: heavy JSON + composite index first, then session markdown,
/// journal, and workspace — avoids workspace/checkpoint markdown ahead of failed heavy writes.
///
/// Cloud uploads are asynchronous; success line includes **pending cloud sync** when Matrix is enabled.
pub(super) fn create_manual_repl_checkpoint(
    state: &mut ReplState,
    label_arg: &str,
) -> Result<String, String> {
    let sid = state
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "No active session — chat once first.".to_string())?;
    let journal = state
        .journal
        .as_ref()
        .ok_or_else(|| "Journal not available.".to_string())?;

    let title = {
        let t = label_arg.trim();
        if t.is_empty() {
            "Manual checkpoint".to_string()
        } else {
            t.to_string()
        }
    };

    let mut ws = astra_services::session_workspace::read_workspace(sid)
        .map_err(|e| format!("read workspace: {e}"))?;
    sync_plan_fields_to_workspace(state, &mut ws);

    let next_step = next_step_checkpoint_number(sid)?;
    let step_cp = build_manual_heavy_step_checkpoint(state, sid);
    let heavy_path =
        persist_manual_heavy_and_composite(sid, ws.turn_count, &title, next_step, &step_cp)?;

    let turn = ws.turn_count;
    let (cp_path, cp_number, cp) =
        persist_manual_session_checkpoint_layer(state, journal, sid, &mut ws, &title)?;

    let cloud_note = if state.matrix_runtime.is_some() {
        " Pending cloud sync — not awaited in the REPL; success is not printed here (errors are logged)."
    } else {
        ""
    };

    spawn_manual_checkpoint_cloud_uploads(state, sid, &cp, next_step, turn, &title, &step_cp);

    Ok(format!(
        "Saved checkpoint #{} (turn {}) — {}; heavy: {}{cloud_note}",
        cp_number,
        turn,
        cp_path.display(),
        heavy_path.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::pipeline::step_checkpoint::read_composite_snapshot_index;
    use astra_runtime::pipeline::step_protocol::StepCheckpoint;

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    #[test]
    fn sync_plan_fields_copies_repl_into_workspace() {
        let mut state = ReplState::default();
        state.executing_plan_goal = Some("goal-x".to_string());
        state.plan_execution_rounds = 9;
        state.plan_execution_corrections = vec!["note".to_string()];

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-plan", "m");
        sync_plan_fields_to_workspace(&state, &mut ws);

        assert_eq!(ws.plan_goal.as_deref(), Some("goal-x"));
        assert_eq!(ws.plan_execution_rounds, 9);
        assert_eq!(ws.plan_corrections, vec!["note".to_string()]);
    }

    #[test]
    fn next_step_checkpoint_number_empty_dir_starts_at_one() {
        let (_tmp, _g) = isolated_sessions_dir();
        assert_eq!(next_step_checkpoint_number("sess-empty").unwrap(), 1);
    }

    #[test]
    fn next_step_checkpoint_number_one_after_max_file() {
        let (tmp, _g) = isolated_sessions_dir();
        let sid = "sess-step";
        let cp_dir = tmp
            .path()
            .join("sessions")
            .join(sid)
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000007-heavy.json"), "{}").unwrap();
        assert_eq!(next_step_checkpoint_number(sid).unwrap(), 8);
    }

    #[test]
    fn manual_heavy_checkpoint_maps_history_to_openai_messages() {
        let mut state = ReplState::default();
        state.history.push(("u1".into(), "a1".into()));
        state.history.push(("u2".into(), "a2".into()));
        state.recent_tools = vec!["bash".to_string()];
        state.turn = 4;
        state.total_prompt_tokens = 11;
        state.total_completion_tokens = 22;
        state.run_id = Some("run-z".to_string());

        let cp = build_manual_heavy_step_checkpoint(&state, "sess-h");
        let StepCheckpoint::Heavy(h) = cp else {
            panic!("expected Heavy checkpoint");
        };
        assert_eq!(h.messages.len(), 4);
        assert_eq!(h.messages[0]["role"], "user");
        assert_eq!(h.messages[0]["content"], "u1");
        assert_eq!(h.messages[3]["content"], "a2");
        assert_eq!(h.recent_tools, vec!["bash".to_string()]);
        assert_eq!(h.light.agent_id, "sess-h");
        assert_eq!(h.light.task_id, "run-z");
        assert_eq!(h.light.total_tokens, 33);
    }

    #[test]
    fn persist_manual_heavy_and_composite_writes_heavy_and_index() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = "sess-heavy-idx";
        let state = ReplState::default();
        let step_cp = build_manual_heavy_step_checkpoint(&state, sid);

        let heavy_path =
            persist_manual_heavy_and_composite(sid, 2, "label-z", 1, &step_cp).unwrap();
        assert!(heavy_path.exists());
        assert!(heavy_path.to_string_lossy().ends_with("-heavy.json"));

        let index = read_composite_snapshot_index(sid).unwrap();
        assert_eq!(index.snapshots.len(), 1);
        assert_eq!(index.snapshots[0].label.as_deref(), Some("manual:label-z"));
    }

    #[test]
    fn persist_manual_session_checkpoint_layer_writes_md_journal_workspace() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        ws.turn_count = 3;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let journal = session_journal::JournalWriter::new(&sid).unwrap();
        let mut state = ReplState::default();
        state.history.push(("hi".into(), "hello".into()));
        state.recent_tools = vec!["read_file".to_string()];

        let mut ws = astra_services::session_workspace::read_workspace(&sid).unwrap();
        let (cp_path, cp_number, _cp) =
            persist_manual_session_checkpoint_layer(&state, &journal, &sid, &mut ws, "decision A")
                .unwrap();

        assert_eq!(cp_number, 1);
        assert!(cp_path.exists());
        assert_eq!(ws.checkpoints, vec![3]);

        let journal_txt =
            std::fs::read_to_string(session_journal::journal_file_path(&sid)).unwrap();
        assert!(
            journal_txt.contains("\"checkpoint\"") || journal_txt.contains("checkpoint"),
            "expected checkpoint journal line, got: {journal_txt:?}"
        );
        assert!(journal_txt.contains("decision"));

        let ws2 = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(ws2.checkpoints, vec![3]);
    }

    #[test]
    fn spawn_manual_cloud_uploads_no_panic_without_matrix() {
        let state = ReplState::default();
        let cp = astra_services::session_checkpoint::Checkpoint {
            number: 1,
            turn: 1,
            title: "t".into(),
            summary: "s".into(),
            tools_used: vec![],
            total_tokens: 0,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        };
        let step_cp = build_manual_heavy_step_checkpoint(&state, "noop");
        spawn_manual_checkpoint_cloud_uploads(&state, "noop", &cp, 1, 1, "t", &step_cp);
    }

    #[test]
    fn short_continuation_prompt_is_detected() {
        assert!(is_short_continuation_prompt("继续"));
        assert!(is_short_continuation_prompt("continue"));
        assert!(is_short_continuation_prompt("resume"));
        assert!(!is_short_continuation_prompt(
            "继续修这个 bug，并顺便看下另一个问题"
        ));
        assert!(!is_short_continuation_prompt("fix this"));
    }

    #[test]
    fn build_effective_line_injects_anchor_for_short_continue() {
        let state = ReplState {
            continuation_anchor: Some(
                "Latest user task: debug Chinese input drops\nLatest assistant direction: inspect prompt redraw path"
                    .to_string(),
            ),
            ..ReplState::default()
        };

        let effective = build_effective_line("继续", &state);
        assert!(effective.contains("[Continuation anchor]"));
        assert!(effective.contains("debug Chinese input drops"));
        assert!(effective.contains("[User follow-up]\n继续"));
    }

    #[test]
    fn build_effective_line_leaves_normal_prompt_untouched() {
        let state = ReplState {
            continuation_anchor: Some("Latest user task: debug Chinese input drops".to_string()),
            ..ReplState::default()
        };

        let effective = build_effective_line("修一下输入法问题", &state);
        assert!(!effective.contains("[Continuation anchor]"));
        assert_eq!(effective, "修一下输入法问题");
    }

    // ── Cost tracking & status line logic tests ──────────────────────────

    #[test]
    fn repl_state_accumulates_cache_tokens_across_turns() {
        let mut state = ReplState::default();
        // Simulate first turn
        state.total_prompt_tokens += 1000;
        state.total_completion_tokens += 500;
        state.total_cache_read_tokens += 800;
        state.total_cache_creation_tokens += 100;
        state.turn += 1;

        // Simulate second turn
        state.total_prompt_tokens += 2000;
        state.total_completion_tokens += 1000;
        state.total_cache_read_tokens += 1500;
        state.total_cache_creation_tokens += 0;
        state.turn += 1;

        assert_eq!(state.total_prompt_tokens, 3000);
        assert_eq!(state.total_completion_tokens, 1500);
        assert_eq!(state.total_cache_read_tokens, 2300);
        assert_eq!(state.total_cache_creation_tokens, 100);
        assert_eq!(state.turn, 2);
    }

    #[test]
    fn cache_hit_percentage_formula() {
        // Formula from print_turn_status_line:
        // cache_pct = cache_read / (prompt + cache_read) * 100
        let prompt = 200u64;
        let cache_read = 800u64;
        let cache_pct =
            cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_zero_when_no_cache() {
        let prompt = 1000u64;
        let cache_read = 0u64;
        let cache_pct =
            cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_100_when_all_cached() {
        let prompt = 0u64;
        let cache_read = 5000u64;
        let cache_pct =
            cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn session_cost_accumulation() {
        let mut state = ReplState::default();
        state.cached_pricing = astra_services::models::PricingData {
            prompt: 3.0,       // $3/1K prompt tokens
            completion: 15.0,  // $15/1K completion tokens
            cache_read: Some(0.3),   // $0.3/1K (10% of prompt)
            cache_write: Some(3.75), // $3.75/1K (125% of prompt)
        };

        // First turn
        let cost1 = crate::cost_for_tokens(1000, 500, 800, 100, &state.cached_pricing);
        state.total_session_cost += cost1;
        assert!(cost1 > 0.0);

        // Second turn
        let cost2 = crate::cost_for_tokens(2000, 1000, 1500, 0, &state.cached_pricing);
        state.total_session_cost += cost2;

        assert!((state.total_session_cost - (cost1 + cost2)).abs() < 1e-10);
    }

    #[test]
    fn token_compact_format_below_1k() {
        let tokens = 999u64;
        let compact = if tokens > 1000 {
            format!("{:.1}k", tokens as f64 / 1000.0)
        } else {
            format!("{tokens}")
        };
        assert_eq!(compact, "999");
    }

    #[test]
    fn token_compact_format_above_1k() {
        let tokens = 12500u64;
        let compact = if tokens > 1000 {
            format!("{:.1}k", tokens as f64 / 1000.0)
        } else {
            format!("{tokens}")
        };
        assert_eq!(compact, "12.5k");
    }
}
