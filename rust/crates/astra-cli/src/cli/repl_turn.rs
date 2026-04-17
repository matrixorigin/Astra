use std::time::Instant;

use astra_services::session_workspace::ContextTraceSignal;
#[cfg(test)]
use astra_services::session_workspace::{ContextTraceBudgetSignal, ContextTraceToolSelection};

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
/// recall (keeps session-relevant context as an anchor after compaction).
const COMPACT_ANCHOR_QUERY_MAX: usize = 220;
const COMPACT_ANCHOR_TOP_K: u32 = 3;
const COMPACT_ANCHOR_MAX_LINES: usize = 3;
const COMPACT_ANCHOR_LINE_MAX: usize = 120;
const COMPACT_ANCHOR_TOTAL_MAX: usize = 400;

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

// ─── User Correction Detection ──────────────────────────────────────────────

/// Correction phrase patterns that indicate user is redirecting/correcting.
const CORRECTION_PATTERNS: &[&str] = &[
    "no,",
    "no i",
    "that's wrong",
    "that's not",
    "i meant",
    "i mean",
    "not that",
    "wrong,",
    "wrong.",
    "incorrect",
    "actually,",
    "actually i",
    "instead,",
    "forget that",
    "ignore that",
    "let me clarify",
    "to clarify",
    "what i want",
    "wait,",
    "hold on",
    "stop,",
    "不对",
    "错了",
    "不是这样",
    "我的意思是",
    "我是说",
    "等等",
    "停一下",
];

/// Detect if a user message appears to be a correction/redirection.
///
/// Returns true if the message contains common correction phrases.
pub(super) fn detect_correction_signal(message: &str) -> bool {
    let msg_lower = message.to_lowercase();
    CORRECTION_PATTERNS.iter().any(|p| msg_lower.contains(p))
}

// ─── Relevance-scored history pruning ───────────────────────────────────────

/// Lightweight tokenizer for relevance scoring: lowercase, split on
/// non-alphanumeric boundaries, filter short tokens.
fn tokenize_for_relevance(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_string())
        .collect()
}

/// Score a single history turn's relevance to a query token set.
///
/// Returns 0.0–1.0 based on keyword overlap (Jaccard-like).
/// Weighs user message higher than assistant response since user messages
/// carry intent and decision context.
fn score_turn_relevance(
    turn: &(String, String),
    query_tokens: &std::collections::HashSet<String>,
) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let user_tokens = tokenize_for_relevance(&turn.0);
    let assistant_tokens = tokenize_for_relevance(&turn.1);

    let user_overlap = query_tokens.intersection(&user_tokens).count();
    let assistant_overlap = query_tokens.intersection(&assistant_tokens).count();

    // User messages carry more intent signal (weight 2x)
    let weighted_overlap = (user_overlap * 2 + assistant_overlap) as f64;
    let max_possible = (query_tokens.len() * 3) as f64; // 2x user + 1x assistant
    (weighted_overlap / max_possible).min(1.0)
}

/// Select which history turns to keep during compaction using relevance scoring.
///
/// Strategy:
/// - Always keep the last `min_recent` turns (guaranteed recency)
/// - From remaining older turns, score by relevance and keep top-K
/// - Returns indices (into original history) that should be PRESERVED as
///   individual turns (not compacted into the summary)
///
/// `keep_budget` is the total number of turns to keep (= `context_budget.keep_recent_turns`).
/// `min_recent` is the minimum guaranteed recent turns (half of keep_budget, at least 2).
fn select_turns_for_compaction(
    history: &[(String, String)],
    keep_budget: usize,
    recent_context: &str,
) -> Vec<usize> {
    let total = history.len();
    if total <= keep_budget {
        return (0..total).collect();
    }

    // Split budget: guaranteed recent + relevance-scored older turns
    let min_recent = (keep_budget / 2).max(2).min(keep_budget);
    let relevance_slots = keep_budget.saturating_sub(min_recent);

    // Guaranteed recent turns (last min_recent)
    let recent_start = total.saturating_sub(min_recent);
    let mut kept_indices: Vec<usize> = (recent_start..total).collect();

    if relevance_slots > 0 && recent_start > 0 {
        // Use the current user message as the primary relevance signal.
        // Recent turns are already guaranteed by min_recent — no need to
        // add their tokens, which would dilute the signal.
        let query_tokens = tokenize_for_relevance(recent_context);

        // Score older turns (0..recent_start) by relevance
        let mut scored: Vec<(usize, f64)> = (0..recent_start)
            .map(|i| (i, score_turn_relevance(&history[i], &query_tokens)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Keep top-K relevant older turns (only if they have nonzero relevance)
        for &(idx, score) in scored.iter().take(relevance_slots) {
            if score > 0.0 {
                kept_indices.push(idx);
            }
        }
    }

    kept_indices.sort_unstable();
    kept_indices
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

    if state.session_id.is_none()
        && let Some(session_id) = state.pending_recovery.clone()
    {
        if is_short_continuation_prompt(&line) {
            if let Err(e) =
                slash_session::restore_session_into_state(&session_id, ctx.profile, state).await
            {
                eprintln!("  {} {}", theme::icon_err(), e.red());
                return Ok(());
            }
        } else {
            state.pending_recovery = None;
        }
    }

    eprintln!();

    // Consume one-shot resume guidance before building the effective line.
    let resume_guidance = state.resume_guidance.take();
    let mut effective_line = build_effective_line(&line, state);
    if let Some(guidance) = resume_guidance {
        effective_line = format!("{guidance}\n\n{effective_line}");
    }
    let turn_start = Instant::now();

    maybe_auto_compact(state, &ctx, token, &effective_line).await?;

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
                    // End the current observability session before creating a new one
                    if let (Some(hub), Some(old_sid)) =
                        (&state.observability_hub, &state.session_id)
                    {
                        let _ = hub.end_session(old_sid);
                    }
                    state.session_id = None;
                    // Unregister stale mailbox to avoid agent_id collision on re-registration
                    state.unregister_root_mailbox().await;
                    state.observability_session = None;
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
                                report_turn_failure(
                                    state,
                                    ctx.profile,
                                    &line,
                                    &retry_failure,
                                    turn_start,
                                );
                                return Ok(());
                            }
                        },
                    }
                }

                // ── 401 auth error: attempt silent token refresh + retry ──
                if is_auth_error(&failure.error) {
                    use crossterm::style::Stylize;
                    eprintln!("{}", "  Token expired, attempting refresh…".yellow());
                    if repl_runtime::attempt_token_refresh(ctx.api, ctx.profile).await {
                        if let Some(new_token) = repl_runtime::current_access_token(ctx.profile) {
                            eprintln!("  {} Token refreshed, retrying…", crate::theme::icon_ok());
                            match run_chat_turn(
                                state,
                                &ctx,
                                &new_token,
                                &effective_line,
                                session_id.as_deref(),
                            )
                            .await
                            {
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
                                    Err(retry_failure) => {
                                        report_turn_failure(
                                            state,
                                            ctx.profile,
                                            &line,
                                            &retry_failure,
                                            turn_start,
                                        );
                                        return Ok(());
                                    }
                                },
                            }
                        }
                    }
                    // Refresh failed — fall through to report original failure.
                }

                report_turn_failure(state, ctx.profile, &line, &failure, turn_start);
            }
        },
    }

    Ok(())
}

pub(super) fn build_effective_line(line: &str, state: &ReplState) -> String {
    let mut effective_line = if let Some(ref dev) = state.skill_dev {
        let skill_md = dev.dir.join("SKILL.md");
        // Re-read SKILL.md from disk every turn so external edits are picked up.
        match std::fs::read_to_string(&skill_md) {
            Ok(source) if !source.trim().is_empty() => format!(
                "{}{line}",
                prompts::build_skill_dev_prefix(
                    &dev.name,
                    &skill_md.display().to_string(),
                    &source,
                )
            ),
            Ok(_) => {
                eprintln!(
                    "  ⚠ SKILL.md is empty at {}, dev context skipped",
                    skill_md.display()
                );
                line.to_string()
            }
            Err(_) => {
                eprintln!(
                    "  ⚠ SKILL.md not found at {}, dev context skipped",
                    skill_md.display()
                );
                line.to_string()
            }
        }
    } else {
        line.to_string()
    };

    if !state.active_system_skills.is_empty() {
        let skill_block = prompts::build_skill_instructions(&state.active_system_skills);
        effective_line = format!("{skill_block}\n\n{effective_line}");
    }

    // Inject project instructions (.astra/instructions.md) if loaded
    if let Some(ref instructions) = state.project_instructions {
        let block = super::format_project_instructions(instructions);
        effective_line = format!("{block}\n\n{effective_line}");
    }

    if let Some(anchor) = state
        .continuation_anchor
        .as_deref()
        .filter(|_| is_short_continuation_prompt(line))
    {
        let goal_line = state
            .session_goal
            .as_deref()
            .map(|g| format!("Session goal: {g}\n"))
            .unwrap_or_default();
        effective_line = format!(
            "[Continuation anchor]\nResume the active task/thread below unless the user explicitly changes topic.\n{goal_line}{anchor}\n\n[User follow-up]\n{effective_line}"
        );
    }

    effective_line
}

pub(super) fn is_short_continuation_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 16 {
        return false;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "continue"
            | "continue."
            | "continue!"
            | "go on"
            | "go ahead"
            | "resume"
            | "do it"
            | "fix it"
            | "try it"
            | "run it"
            | "yes"
            | "yes."
            | "ok"
            | "ok."
            | "okay"
            | "sure"
            | "proceed"
            | "next"
            | "keep going"
    ) || matches!(
        trimmed,
        "继续" | "继续。" | "继续！" | "好的" | "好" | "可以" | "是的" | "对" | "行" | "嗯"
    )
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
    current_message: &str,
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
    let obs_hub = state.observability_hub.clone();
    let obs_session = state.observability_session.clone();
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
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: ctx.selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: &state.unified_skill_registry,
        plan_only_chat: false,
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
        messaging_metrics: state.messaging_metrics.clone(),
        agent_spawner: state.agent_spawner.clone(),
        root_agent_id: Some("main"),
        root_mailbox_slot: Some(&mut state.root_mailbox),
        observability_hub: obs_hub,
        observability_session: obs_session,
        file_journal: Some(state.file_journal.clone()),
        database_snapshot_journal: Some(state.database_snapshot_journal.clone()),
        git_stash_journal: Some(state.git_stash_journal.clone()),
        git_commit_journal: Some(state.git_commit_journal.clone()),
        git_worktree_journal: Some(state.git_worktree_journal.clone()),
        session_state_journal: Some(state.session_state_journal.clone()),
        task_manager: Some(state.task_manager.clone()),
        turn_index: state.turn,
        evolution_service: state.evolution_service.clone(),
    })
    .await;

    if let Ok(result) = compact_result {
        apply_auto_compact_result(state, ctx, token, result, current_message).await?;
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
    current_message: &str,
) -> Result<(), String> {
    let summary = result.full_text.trim().to_string();
    if summary.is_empty() {
        return Ok(());
    }

    let keep = state.context_budget.keep_recent_turns;
    let total = state.history.len();
    if total <= keep {
        return Ok(());
    }

    // Use relevance scoring to decide which turns to preserve as individual
    // messages vs. compact into the summary.
    let kept_indices = select_turns_for_compaction(&state.history, keep, current_message);
    let compacted_count = total - kept_indices.len();
    if compacted_count == 0 {
        return Ok(());
    }

    // Skip Memoria anchor for short sessions — the summary alone is sufficient
    // and the anchor would largely duplicate what's already in kept turns.
    let anchor = if compacted_count >= 4 {
        fetch_compact_memory_anchor_snippet(ctx.api, token, state.session_id.as_deref(), &summary)
            .await
    } else {
        None
    };
    let assistant_text = compact_assistant_message(compacted_count, &summary, anchor.as_deref());
    let summary_entry = (String::new(), assistant_text);
    let mut new_history = vec![summary_entry];

    // Append the preserved turns (in original order)
    for &idx in &kept_indices {
        new_history.push(state.history[idx].clone());
    }

    // Record which turns were compacted for drift detection.
    // Turns not in kept_indices were compacted.
    for i in 0..total {
        if !kept_indices.contains(&i) {
            state.drift_compressed_turns.push(i as u32);
        }
    }

    state.history = new_history;
    state.recent_tools.clear();

    let entry = prompts::memory_proto::MemoryEntry::new(
        prompts::memory_proto::NS_EPISODE,
        prompts::memory_proto::ST_AUTO,
        &summary,
    );
    let meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
        state.session_id.as_deref(),
        state.turn,
        prompts::memory_proto::SRC_AUTO_COMPACT,
        prompts::memory_proto::TIER_INFERRED,
    );
    if let Err(e) = ctx
        .api
        .post_memory_store_json(token, &entry.to_store_payload_with_meta(&meta))
        .await
    {
        astra_core::agent_warn!("repl_turn", "failed to persist compacted memory: {e}");
    }

    // Write compact event with summary to journal so finalize_workspace_on_end
    // can populate the workspace summary for P1/P2 knowledge backflow.
    if let Some(ref journal) = state.journal {
        let evt = session_journal::JournalEvent::compact_with_summary(
            state.session_id.as_deref(),
            state.turn,
            compacted_count,
            0, // facts_stored — auto-compact doesn't extract facts
            Some(&summary),
        );
        let _ = journal.append(&evt);
    }

    // ─── Observability: record history compression decision ───────────────
    if let Some(session) = &state.observability_session {
        use astra_runtime::turn::decision_explainer::{
            DecisionExplanation, DecisionType, ExplainableInput,
        };
        let compacted_turns: Vec<u32> = (0..(total - kept_indices.len()) as u32).collect();
        let retained_turns: Vec<u32> = kept_indices.iter().map(|&i| i as u32).collect();
        let compression_ratio = if total > 0 {
            compacted_count as f64 / total as f64
        } else {
            0.0
        };
        let trigger_tokens = state.context_budget.compact_trigger();
        let explanation = DecisionExplanation {
            id: format!(
                "compact-{}-{}",
                state.session_id.as_deref().unwrap_or("?"),
                state.turn
            ),
            timestamp: std::time::SystemTime::now(),
            decision_type: DecisionType::HistoryCompression {
                turns_compressed: compacted_turns,
                turns_retained: retained_turns.clone(),
                compression_ratio,
            },
            inputs: vec![ExplainableInput {
                name: "token_budget".to_string(),
                value: format!("{}k trigger", trigger_tokens / 1000),
                influence: 1.0,
                explanation: Some("Exceeded context budget trigger".to_string()),
            }],
            reasoning: format!(
                "Auto-compacted {} turns to {} (kept {} by relevance), ratio {:.1}%",
                total,
                state.history.len(),
                retained_turns.len(),
                compression_ratio * 100.0
            ),
            alternatives: vec![],
            confidence: 0.9,
        };
        let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
        astra_runtime::observability_integration::on_tool_selection(
            &mut session_guard,
            explanation,
        );
    }

    // Report which turns were kept by relevance (if any older turns survived)
    let recent_start = total.saturating_sub(keep / 2);
    let relevance_kept: Vec<usize> = kept_indices
        .iter()
        .filter(|&&i| i < recent_start)
        .copied()
        .collect();
    let relevance_note = if relevance_kept.is_empty() {
        String::new()
    } else {
        format!(
            " ({} older turn{} kept by relevance)",
            relevance_kept.len(),
            if relevance_kept.len() == 1 { "" } else { "s" }
        )
    };

    eprintln!(
        "  {}",
        format!(
            "{} Compacted {compacted_count} turns → {} in context{relevance_note}",
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

    // ─── Drift Tracking: Record original query on first turn ────────────────
    if state.drift_original_query.is_none() {
        state.drift_original_query = Some(message.to_string());
    }

    // ─── Drift Tracking: Detect user corrections ────────────────────────────
    // If this message looks like a correction, record the current turn index.
    if detect_correction_signal(message) {
        let correction_turn = state.history.len() as u32;
        state.drift_user_corrections.push(correction_turn);
    }

    // Create a cancellation token that can interrupt SSE streaming mid-flight.
    let cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    let cancel_token_for_signal = cancel_token.clone();

    // Clone observability context for the turn
    let obs_hub = state.observability_hub.clone();
    let obs_session = state.observability_session.clone();

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
            render_policy: crate::stream_render::RenderPolicy::Stream,
            selector: ctx.selector,
            recent_tools: &state.recent_tools,
            tool_health_entries: &state.tool_health_entries,
            unified_skill_registry: &state.unified_skill_registry,
            plan_only_chat: state.chat_plan_only && state.current_plan_subtask_id.is_none(),
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
            messaging_metrics: state.messaging_metrics.clone(),
            agent_spawner: state.agent_spawner.clone(),
            root_agent_id: Some("main"),
            root_mailbox_slot: Some(&mut state.root_mailbox),
            observability_hub: obs_hub,
            observability_session: obs_session,
            file_journal: Some(state.file_journal.clone()),
            database_snapshot_journal: Some(state.database_snapshot_journal.clone()),
            git_stash_journal: Some(state.git_stash_journal.clone()),
            git_commit_journal: Some(state.git_commit_journal.clone()),
            git_worktree_journal: Some(state.git_worktree_journal.clone()),
            session_state_journal: Some(state.session_state_journal.clone()),
            task_manager: Some(state.task_manager.clone()),
            turn_index: state.turn,
            evolution_service: state.evolution_service.clone(),
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
    learning_snap: &ReplTurnLearningSnapshot,
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
        .with_selector_learning_telemetry(
            result.selector_confidence,
            result.routing_domain_hint.clone(),
            result.entity_learn_skipped_no_domain,
        )
        .with_memoria_time(result.memoria_ms)
        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);

        // Store for /turn command
        state.last_turn_event = Some(turn_event.clone());

        if let Err(e) = journal.append(&turn_event) {
            astra_core::agent_warn!("journal", "failed to write turn event: {e}");
        }
        enqueue_ingestion(state, &turn_event);

        // Emit deferred context_assembly_recorded — only on successful turn commit.
        if let Some((turn_num, trace_json)) = &result.pending_context_assembly_trace {
            let assembly_event = session_journal::JournalEvent::context_assembly_recorded(
                state.session_id.as_deref(),
                *turn_num,
                trace_json.clone(),
            );
            if let Err(e) = journal.append(&assembly_event) {
                astra_core::agent_warn!(
                    "journal",
                    "failed to write deferred context_assembly event: {e}"
                );
            }
        }

        // Update workspace metadata per-turn
        if let Some(sid) = state.session_id.as_deref()
            && let Ok(mut ws) = astra_services::session_workspace::read_workspace(sid)
        {
            ws.record_turn(result.prompt_tokens, result.completion_tokens);

            // Persist plan state to workspace for session resume
            sync_plan_fields_to_workspace(state, &mut ws);
            sync_context_trace_to_workspace(state, &mut ws);
            sync_session_state_to_workspace(state, &mut ws);

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

            if let Some(ref mc) = state.matrix_runtime
                && let Some(ref trace_signal) = ws.last_context_trace
            {
                let user_id = state.ingestion_user_id.as_deref().unwrap_or("anonymous");
                let pool = mc.shared_pool().get().clone();
                let sid_owned = sid.to_string();
                let user_id_owned = user_id.to_string();
                let trace_signal = trace_signal.clone();
                tokio::spawn(async move {
                    let _ = astra_services::session_restore::push_context_trace_signal_to_cloud(
                        &pool,
                        &sid_owned,
                        &user_id_owned,
                        &trace_signal,
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
                    let _ = astra_services::session_restore::push_session_state_to_cloud(
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

            if let Err(e) = astra_services::session_workspace::write_workspace(&ws) {
                eprintln!("  ⚠ workspace write failed after stall detection: {e}");
            }
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
                &ve.deprioritized_tools,
                ve.force_stop,
                ve.nudge_count,
                ve.total_errors,
                ve.deprioritized_count,
                ve.total_timeouts,
                &ve.timeout_dominant_tools,
                ve.total_cache_hits,
                ve.flaky_count,
            );
            let _ = journal.append(&verdict_event);
            enqueue_ingestion(state, &verdict_event);
        }

        let turn_eval_event =
            astra_runtime::pipeline::evaluation::build_turn_evaluation_journal_event(
                state.session_id.as_deref(),
                Some(state.turn),
                "cli_repl",
                line,
                &state.recent_tools,
                &result.tool_call_records,
                result.stall_events.len(),
                result.verdict_events.iter().any(|event| {
                    event.severity.eq_ignore_ascii_case("warning")
                        || event.severity.eq_ignore_ascii_case("critical")
                }),
                result.budget_pressure,
                &learning_snap.eval,
            );
        let _ = journal.append(&turn_eval_event);
        enqueue_ingestion(state, &turn_eval_event);

        // Step Protocol recorder summary: previously emitted as a second
        // checkpoint event, causing duplicate checkpoint entries with
        // inconsistent token counts. The summary is already captured in the
        // main checkpoint's `cp.summary` when the interval fires, so we
        // skip the duplicate event.
    }
}

/// Routing + turn quality for journal fields and `ToolSelector::record_outcome`.
pub(super) struct ReplTurnLearningSnapshot {
    pub routing: astra_runtime::pipeline::routing::RoutingDecision,
    pub eval: astra_runtime::pipeline::evaluation::TurnEvaluation,
}

pub(super) fn analyze_repl_turn_learning(
    line: &str,
    turn: u32,
    recent_tools: &[String],
    result: &StreamResult,
) -> ReplTurnLearningSnapshot {
    use astra_runtime::pipeline::evaluation::evaluate_tool_call_records;
    use astra_runtime::pipeline::routing::RoutingEngine;
    let routing = RoutingEngine::analyze(line, turn, recent_tools, &[], vec![]);

    let has_verdict_warning = result.verdict_events.iter().any(|v| {
        v.severity.eq_ignore_ascii_case("warning") || v.severity.eq_ignore_ascii_case("critical")
    });

    let eval = evaluate_tool_call_records(
        line,
        recent_tools,
        &result.tool_call_records,
        result.stall_events.len(),
        has_verdict_warning,
        result.budget_pressure,
    );

    ReplTurnLearningSnapshot { routing, eval }
}

fn record_selector_turn_outcome(
    selector: &dyn tool_selector::ToolSelector,
    line: &str,
    result: &StreamResult,
    snap: &ReplTurnLearningSnapshot,
    prev_assistant_text: Option<&str>,
) {
    let signal = astra_runtime::turn::implicit_feedback::detect_implicit_feedback_signal(
        line,
        prev_assistant_text,
    );
    let was_corrected = matches!(signal.signal_type.as_str(), "correction" | "frustration");
    selector.record_outcome(
        line,
        &result.tools_used,
        snap.routing.task_type,
        snap.routing.domain_hint,
        snap.eval.success,
        snap.eval.quality,
        was_corrected,
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

        // Initialize observability session if hub is available and session not yet created
        if state.observability_session.is_none() {
            if let Some(hub) = &state.observability_hub {
                let user_id = state
                    .ingestion_user_id
                    .clone()
                    .unwrap_or_else(|| "anonymous".to_string());
                let obs_session = hub.start_session(&user_id, session_id);
                state.observability_session = Some(obs_session);
                apply_pending_adaptive_state(state);
                apply_pending_goal_progress_state(state);
            }
        }
    }

    state.turn += 1;
    state.total_prompt_tokens += result.prompt_tokens;
    state.total_completion_tokens += result.completion_tokens;
    state.total_cache_read_tokens += result.cache_read_tokens;
    state.total_cache_creation_tokens += result.cache_creation_tokens;

    // Accumulate per-turn cost
    let turn_cost = crate::slash_stats::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );
    state.total_session_cost += turn_cost;
    state.last_response = Some(result.full_text.clone());
    state.continuation_anchor = build_continuation_anchor(state, line, &result);
    state.pending_followup_suggestion =
        crate::followup_suggestion::suggest_followup(line, state, &result);
    if let Some(suggestion) = state.pending_followup_suggestion.as_ref() {
        super::repl_ui::set_followup_prompt_hint(Some(suggestion.text.clone()));
    } else {
        super::repl_ui::clear_followup_prompt_hint();
    }

    // Capture session goal from the first substantive user message.
    if state.session_goal.is_none() && !line.trim().is_empty() {
        let goal: String = line.trim().chars().take(220).collect();
        state.session_goal = Some(goal);
    }
    // New user input invalidates redo stack (history diverged)
    state.redo_stack.clear();
    state
        .history
        .push((line.to_string(), result.full_text.clone()));
    state.recent_tools = result.tools_used.clone();

    // Persist tool health for cross-session error budgets
    if !result.tool_health_export.is_empty() {
        state.tool_health_entries = result.tool_health_export.clone();
    }

    let learning_snap = analyze_repl_turn_learning(line, state.turn, &state.recent_tools, &result);
    let mut result = result;
    let routing_domain = learning_snap
        .routing
        .domain_hint
        .map(|d| astra_runtime::pipeline::routing::domain_hint_to_label(d).to_string());
    let entity_skipped = learning_snap.eval.success
        && !result.tools_used.is_empty()
        && learning_snap.routing.domain_hint.is_none();
    result.set_repl_learning_journal_fields(routing_domain, entity_skipped);

    commit_turn_journal_workspace_and_sidecars(state, line, &result, &learning_snap, turn_start);
    // Previous assistant response = second-to-last history entry (last is current turn,
    // already pushed above). Used to detect if the user is correcting the prior response.
    let prev_assistant_text = state
        .history
        .len()
        .checked_sub(2)
        .and_then(|i| state.history.get(i))
        .map(|(_, resp)| resp.as_str());
    record_selector_turn_outcome(selector, line, &result, &learning_snap, prev_assistant_text);

    // ── Skill auto-improvement check ─────────────────────────────────────
    check_skill_improvement(state, line, &result);

    // ── Post-turn status line ────────────────────────────────────────────
    print_turn_status_line(state, &result, turn_start);
    if let Some(suggestion) = state.pending_followup_suggestion.as_ref() {
        eprintln!(
            "{}",
            format!("  💡 Next prompt: {}  (Tab to accept)", suggestion.text).dim()
        );
    }

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
    let turn_cost = crate::slash_stats::cost_for_tokens(
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
        parts.push(crate::slash_stats::format_cost(turn_cost));
    }

    parts.push(elapsed_str);

    // TTFT (Time To First Token) - valuable for understanding API latency
    if let Some(ttft) = result.ttft_ms {
        if ttft > 0 {
            parts.push(format!("ttft:{ttft}ms"));
        }
    }

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
        let cache_pct = result.cache_read_tokens as f64
            / (result.prompt_tokens + result.cache_read_tokens).max(1) as f64
            * 100.0;
        parts.push(format!("cache:{cache_pct:.0}%"));
    }

    let line = format!("  ─ {} ─", parts.join(" │ "));
    eprintln!("{}", line.dim());

    // Session total on second line (only after first turn with pricing)
    let session_cost = state.total_session_cost + turn_cost;
    if session_cost > 0.0 && state.turn > 0 {
        let session_line = format!(
            "  session: {}",
            crate::slash_stats::format_cost(session_cost)
        );
        eprintln!("{}", session_line.dim());
    }

    // Context window warning at 70% and 85% budget pressure
    print_context_window_warning(result.budget_pressure);

    let w = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80);
    let rule = "─".repeat(w.min(72));
    eprintln!("{}", rule.dim());
}

/// Print a context window warning when budget pressure exceeds thresholds.
///
/// - 70-84%: Yellow warning suggesting cleanup or summarization
/// - 85%+: Red warning indicating risk of context overflow
fn print_context_window_warning(budget_pressure: f64) {
    const WARNING_THRESHOLD: f64 = 0.70;
    const CRITICAL_THRESHOLD: f64 = 0.85;

    if budget_pressure >= CRITICAL_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🔴 Context window {:.0}% full ({:.0}% remaining) — consider /compact or starting a new session",
                budget_pressure * 100.0,
                remaining
            )
            .red()
        );
    } else if budget_pressure >= WARNING_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🟡 Context window {:.0}% used ({:.0}% remaining) — use /stats context for details",
                budget_pressure * 100.0,
                remaining
            )
            .yellow()
        );
    }
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
    astra_core::agent_debug!(
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
    let target_path = session_journal::journal_file_path(session_id);
    let already_attached = state
        .journal
        .as_ref()
        .map(|journal| journal.path() == &target_path)
        .unwrap_or(false);

    if !already_attached {
        state.journal = match session_journal::JournalWriter::new(session_id) {
            Ok(journal) => Some(journal),
            Err(err) => {
                eprintln!(
                    "{}",
                    format!("  ⚠ Session journal not available for {session_id}: {err}").yellow()
                );
                None
            }
        };
    }

    let needs_start_event = match session_journal::read_journal(session_id) {
        Ok(events) => matches!(
            events.last().map(|event| &event.event_type),
            None | Some(session_journal::JournalEventType::SessionEnd)
        ),
        Err(_) => true,
    };

    if !already_attached && needs_start_event {
        let Some(journal) = state.journal.as_ref() else {
            return;
        };
        let start_event =
            session_journal::JournalEvent::session_start(Some(session_id), state.model.as_deref());
        let _ = journal.append(&start_event);
        // enqueue_ingestion skips if matrix_runtime is None
        enqueue_ingestion(state, &start_event);
    }

    // Keep workspace metadata in sync without resetting accumulated counters.
    let (mut ws, mut dirty, workspace_existed) =
        match astra_services::session_workspace::read_workspace(session_id) {
            Ok(ws) => (ws, false, true),
            Err(_) => (
                astra_services::session_workspace::WorkspaceMetadata::new(
                    session_id,
                    state.model.as_deref().unwrap_or("default"),
                ),
                true,
                false,
            ),
        };
    if ws.status != "active" {
        ws.status = "active".to_string();
        dirty = true;
    }
    // Preserve the workspace model for existing sessions so `/session` can report
    // what the session originally started as even if the live model changes later.
    if let Some(model) = state.model.as_deref()
        && (ws.model.is_empty() || (!workspace_existed && ws.model != model))
    {
        ws.model = model.to_string();
        dirty = true;
    }
    if dirty {
        ws.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(e) = astra_services::session_workspace::write_workspace(&ws) {
            eprintln!("  ⚠ workspace write failed during init: {e}");
        }
    }

    // Initialize observability session for context tracing (M1).
    // This enables TurnTraceCollector creation in the agentic loop.
    if state.observability_session.is_none() {
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple(session_id),
        )));
        apply_pending_adaptive_state(state);
        apply_pending_goal_progress_state(state);
    }
}

/// Report a turn failure with enriched partial data from the agentic loop.
/// Detect 401 / credential-expired errors from a turn failure message.
pub(super) fn is_auth_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    error.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("could not validate credentials")
}

fn report_turn_failure(
    state: &mut ReplState,
    profile: Option<&str>,
    line: &str,
    failure: &crate::TurnFailure,
    turn_start: Instant,
) {
    if is_auth_error(&failure.error) {
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
        if let Some(sid) = failure
            .partial
            .session_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
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
        crate::streaming_types::apply_partial_turn_data_to_error_event(
            &mut err_event,
            &failure.partial,
        );
        // Always populate metadata for post-mortem analysis.
        {
            let error_kind = astra_core::ClassifiedError::from(failure.error.clone()).kind;
            err_event.metadata = Some(serde_json::json!({
                "error_kind": error_kind.as_str(),
                "retryable": error_kind.is_retryable(),
                "guidance": error_kind.guidance(),
                "stall_count": failure.partial.stall_events.len(),
                "verdict_count": failure.partial.verdict_events.len(),
                "has_checkpoint": failure.partial.last_heavy_checkpoint.is_some(),
                "partial_tokens_in": failure.partial.prompt_tokens,
                "partial_tokens_out": failure.partial.completion_tokens,
                "partial_tool_calls": failure.partial.tool_calls_count,
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
        state.history.push((line.to_string(), partial_with_note));
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

/// Sync session state fields to workspace for resume capability.
fn sync_session_state_to_workspace(
    state: &ReplState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
    ws.session_goal = state.session_goal.clone();
    ws.goal_progress = None;
    ws.pinned_skills = state.pinned_skills.iter().cloned().collect();
    ws.discovered_skills = state.discovered_skills.iter().cloned().collect();

    // Persist adaptive engine state (anti-flap, experiment, tuned config)
    if let Some(obs) = &state.observability_session {
        if let Ok(guard) = obs.read() {
            ws.goal_progress = guard.goal_progress_snapshot().filter(|progress| {
                state
                    .session_goal
                    .as_deref()
                    .map(|goal| goal == progress.goal.as_str())
                    .unwrap_or(true)
            });
            if ws.session_goal.is_none() {
                ws.session_goal = ws
                    .goal_progress
                    .as_ref()
                    .map(|progress| progress.goal.clone());
            }
            ws.last_scenario_change_turn = guard.last_scenario_change_turn;
            ws.last_token_budget_direction = guard.last_token_budget_direction;
            ws.last_token_budget_change_turn = guard.last_token_budget_change_turn;
            ws.active_experiment_id = guard.active_experiment_id.clone();
            ws.active_variant = guard.active_variant.clone();
            ws.tuned_config_json = serde_json::to_string(&guard.config).ok();
        }
    }
}

pub(super) struct GoalSteeringChange {
    pub previous_goal: Option<String>,
    pub turn: u32,
}

pub(super) fn steer_observability_goal(
    state: &mut ReplState,
    goal: &str,
) -> Option<GoalSteeringChange> {
    let session_turn = state.turn;
    let obs = state.observability_session.as_ref()?;
    let mut guard = obs.write().unwrap_or_else(|error| error.into_inner());
    let previous_goal = guard
        .goal_tracker
        .as_ref()
        .map(|tracker| tracker.goal().to_string())
        .or_else(|| guard.original_query.clone());
    if !guard.steer_goal(goal) {
        return None;
    }
    Some(GoalSteeringChange {
        previous_goal,
        turn: session_turn,
    })
}

/// Apply persisted adaptive engine state to a newly created ObservabilitySession.
/// Called when pending_adaptive_state was stashed during workspace restore and the
/// ObservabilitySession is now available to receive it.
pub(super) fn apply_pending_adaptive_state(state: &mut ReplState) {
    let adaptive = match state.pending_adaptive_state.take() {
        Some(a) => a,
        None => return,
    };
    let obs = match &state.observability_session {
        Some(o) => o,
        None => {
            state.pending_adaptive_state = Some(adaptive);
            return;
        }
    };
    let mut guard = match obs.write() {
        Ok(guard) => guard,
        Err(_) => {
            state.pending_adaptive_state = Some(adaptive);
            return;
        }
    };
    guard.last_scenario_change_turn = adaptive.last_scenario_change_turn;
    guard.last_token_budget_direction = adaptive.last_token_budget_direction;
    guard.last_token_budget_change_turn = adaptive.last_token_budget_change_turn;
    if adaptive.active_experiment_id.is_some() {
        guard.active_experiment_id = adaptive.active_experiment_id;
        guard.active_variant = adaptive.active_variant;
    }
    // Restore tuned RuntimeConfig (merge on top of freshly loaded defaults)
    if let Some(json) = &adaptive.tuned_config_json {
        if let Ok(saved_config) =
            serde_json::from_str::<astra_runtime::runtime_config::RuntimeConfig>(json)
        {
            let current = std::mem::take(&mut guard.config);
            guard.config = current.merge(saved_config);
        }
    }
}

pub(super) fn apply_pending_goal_progress_state(state: &mut ReplState) {
    let goal_progress = match state.pending_goal_progress.take() {
        Some(progress) => progress,
        None => return,
    };
    if state
        .session_goal
        .as_deref()
        .map(|goal| goal != goal_progress.goal.as_str())
        .unwrap_or(false)
    {
        // Deliberately drop stale progress snapshots when the tracked goal no longer
        // matches the current session goal.
        return;
    }
    let obs = match &state.observability_session {
        Some(session) => session,
        None => {
            state.pending_goal_progress = Some(goal_progress);
            return;
        }
    };
    match obs.write() {
        Ok(mut guard) => guard.restore_goal_progress(goal_progress),
        Err(_) => {
            state.pending_goal_progress = Some(goal_progress);
        }
    }
}

fn latest_context_trace_signal(state: &ReplState) -> Option<ContextTraceSignal> {
    let obs = state.observability_session.as_ref()?;
    let guard = obs.read().ok()?;
    astra_runtime::observability_integration::latest_context_trace_signal(&guard)
}

fn sync_context_trace_to_workspace(
    state: &ReplState,
    ws: &mut astra_services::session_workspace::WorkspaceMetadata,
) {
    ws.last_context_trace = latest_context_trace_signal(state);
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
        budget_remaining_tokens: {
            let limit = astra_core::RuntimeLimits::global().max_turn_input_tokens;
            if limit == 0 {
                0 // unlimited
            } else {
                limit.saturating_sub(state.total_prompt_tokens)
            }
        },
        budget_remaining_rounds: max_turns.saturating_sub(state.turn),
        blocked_tools: Vec::new(),
        recent_tools: state.recent_tools.clone(),
        learning_snapshot_id: None,
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: Vec::new(),
        interruption: None,
        approval_overrides: None,
        consecutive_context_window_errors: 0,
        compaction_state: None,
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

    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.to_string(), turn)
            .label(format!("manual:{title}"))
            .session_state(format!("{next_step:06}-heavy.json"))
            .workspace_state(sid.to_string())
            .build();
    let mut index = read_composite_snapshot_index(sid).unwrap_or_default();
    index
        .append(&mut snapshot)
        .map_err(|e| format!("append snapshot version: {e}"))?;
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
#[derive(Debug, Clone)]
pub(super) struct ManualCheckpointSummary {
    pub checkpoint_number: u32,
    pub turn: u32,
    pub checkpoint_path: std::path::PathBuf,
    pub heavy_path: std::path::PathBuf,
    pub cloud_sync_queued: bool,
}

impl ManualCheckpointSummary {
    pub fn headline(&self) -> String {
        format!(
            "Checkpoint #{} saved (turn {})",
            self.checkpoint_number, self.turn
        )
    }
}

pub(super) fn create_manual_repl_checkpoint(
    state: &mut ReplState,
    label_arg: &str,
) -> Result<ManualCheckpointSummary, String> {
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
    sync_context_trace_to_workspace(state, &mut ws);

    let next_step = next_step_checkpoint_number(sid)?;
    let step_cp = build_manual_heavy_step_checkpoint(state, sid);
    let heavy_path =
        persist_manual_heavy_and_composite(sid, ws.turn_count, &title, next_step, &step_cp)?;

    let turn = ws.turn_count;
    let (cp_path, cp_number, cp) =
        persist_manual_session_checkpoint_layer(state, journal, sid, &mut ws, &title)?;

    spawn_manual_checkpoint_cloud_uploads(state, sid, &cp, next_step, turn, &title, &step_cp);

    Ok(ManualCheckpointSummary {
        checkpoint_number: cp_number,
        turn,
        checkpoint_path: cp_path,
        heavy_path,
        cloud_sync_queued: state.matrix_runtime.is_some(),
    })
}

/// Classify a turn error message into an [`ErrorKind`] for post-mortem analysis.
///
/// Delegates to [`astra_core::classify_tool_output`] — kept as a thin wrapper
/// for backward compatibility with callers that haven't migrated yet.
#[allow(dead_code)]
fn classify_turn_error(error: &str) -> astra_core::ErrorKind {
    astra_core::classify_tool_output(error)
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

    fn poisoned_observability_session(
        session_id: &str,
    ) -> std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    > {
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple(session_id),
        ));
        let poisoned = session.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.write().unwrap();
            panic!("poison observability lock");
        }));
        session
    }

    #[test]
    fn initialize_journal_attaches_without_duplicate_start_or_workspace_reset() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-attach-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                3,
                None,
                "hello",
                "world",
                0,
                10,
                5,
                20,
            ))
            .unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 3;
        ws.total_tokens_in = 10;
        ws.total_tokens_out = 5;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = ReplState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            1,
        );

        let restored_ws = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(restored_ws.turn_count, 3);
        assert_eq!(restored_ws.total_tokens_in, 10);
        assert_eq!(restored_ws.total_tokens_out, 5);
        assert_eq!(restored_ws.status, "active");
    }

    #[test]
    fn initialize_journal_reopens_completed_session_without_resetting_workspace() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-reopen-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 3))
            .unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 3;
        ws.total_tokens_in = 120;
        ws.total_tokens_out = 45;
        ws.status = "completed".to_string();
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = ReplState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            2,
        );
        assert_eq!(
            events.last().map(|event| &event.event_type),
            Some(&session_journal::JournalEventType::SessionStart)
        );

        let restored_ws = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(restored_ws.turn_count, 3);
        assert_eq!(restored_ws.total_tokens_in, 120);
        assert_eq!(restored_ws.total_tokens_out, 45);
        assert_eq!(restored_ws.status, "active");
    }

    #[test]
    fn initialize_journal_does_not_duplicate_start_after_sync_marker() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-sync-marker-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "world",
                0,
                10,
                5,
                20,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::cloud_pull_sync_marker(
                Some(&sid),
                "default",
                "repl_startup",
                Some(3),
                true,
                2,
                &["blocked_tools".to_string()],
                false,
            ))
            .unwrap();

        let mut state = ReplState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            1,
        );
        assert_eq!(
            events.last().map(|event| &event.event_type),
            Some(&session_journal::JournalEventType::SyncMarker)
        );
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
    fn sync_session_state_persists_goal_progress() {
        let mut state = ReplState::default();
        state.session_goal = Some("ship auth".to_string());
        let mut obs =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-goal");
        obs.record_query("ship auth");
        obs.record_tool_result(
            "bash",
            "test result: ok. 12 passed; 0 failed; 0 ignored",
            Some(0),
        );
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-goal", "m");
        sync_session_state_to_workspace(&state, &mut ws);

        let progress = ws.goal_progress.expect("goal progress should persist");
        assert_eq!(progress.goal, "ship auth");
        assert_eq!(progress.milestone_count, 1);
        assert!(progress.completion_score > 0.0);
    }

    #[test]
    fn apply_pending_goal_progress_restores_observability_tracker() {
        let mut source =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-src");
        source.record_query("ship auth");
        source.record_tool_result("bash", "Finished `dev` profile", Some(0));
        let snapshot = source
            .goal_progress_snapshot()
            .expect("goal progress snapshot");

        let mut state = ReplState::default();
        state.session_goal = Some(snapshot.goal.clone());
        state.pending_goal_progress = Some(snapshot.clone());
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-dst"),
        )));

        apply_pending_goal_progress_state(&mut state);

        let restored = state
            .observability_session
            .as_ref()
            .unwrap()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .goal_progress()
            .expect("restored progress");
        assert_eq!(restored.milestone_count, snapshot.milestone_count);
        assert!(state.pending_goal_progress.is_none());
    }

    #[test]
    fn apply_pending_goal_progress_requeues_when_lock_is_poisoned() {
        let mut source =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-src");
        source.record_query("ship auth");
        source.record_tool_result("bash", "Finished `dev` profile", Some(0));
        let snapshot = source
            .goal_progress_snapshot()
            .expect("goal progress snapshot");

        let mut state = ReplState::default();
        state.session_goal = Some(snapshot.goal.clone());
        state.pending_goal_progress = Some(snapshot.clone());
        state.observability_session = Some(poisoned_observability_session("sid-poisoned"));

        apply_pending_goal_progress_state(&mut state);

        assert_eq!(state.pending_goal_progress, Some(snapshot));
    }

    #[test]
    fn apply_pending_adaptive_state_requeues_when_lock_is_poisoned() {
        let mut state = ReplState::default();
        state.pending_adaptive_state = Some(super::repl_state::PersistedAdaptiveState {
            last_scenario_change_turn: Some(3),
            last_token_budget_direction: 1,
            last_token_budget_change_turn: Some(2),
            active_experiment_id: Some("exp-1".to_string()),
            active_variant: Some("variant-a".to_string()),
            tuned_config_json: None,
        });
        state.observability_session = Some(poisoned_observability_session("sid-adaptive"));

        apply_pending_adaptive_state(&mut state);

        let adaptive = state
            .pending_adaptive_state
            .as_ref()
            .expect("adaptive state should remain pending");
        assert_eq!(adaptive.last_token_budget_direction, 1);
        assert_eq!(adaptive.active_experiment_id.as_deref(), Some("exp-1"));
    }

    #[test]
    fn steer_observability_goal_updates_live_tracker() {
        let mut state = ReplState::default();
        let mut obs =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-steer");
        obs.record_query("finish auth flow");
        obs.compressed_turns.push(2);
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        steer_observability_goal(&mut state, "ship billing flow");

        let guard = state
            .observability_session
            .as_ref()
            .unwrap()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.original_query.as_deref(), Some("ship billing flow"));
        assert_eq!(
            guard.goal_tracker.as_ref().map(|tracker| tracker.goal()),
            Some("ship billing flow")
        );
        assert_eq!(guard.recent_queries, vec!["ship billing flow".to_string()]);
        assert!(guard.compressed_turns.is_empty());
    }

    #[test]
    fn steer_observability_goal_reports_session_turn_not_internal_loop_turn() {
        let mut state = ReplState {
            turn: 3,
            ..Default::default()
        };
        let mut obs =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-steer");
        obs.turn_number = 6;
        obs.record_query("review session health");
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        let change = steer_observability_goal(&mut state, "run plan execution").expect("change");

        assert_eq!(change.turn, 3);
        assert_eq!(
            change.previous_goal.as_deref(),
            Some("review session health")
        );
    }

    #[test]
    fn sync_context_trace_copies_latest_trace_into_workspace() {
        let mut state = ReplState::default();
        let mut obs =
            astra_runtime::observability_integration::ObservabilitySession::new_simple("sid-trace");
        obs.context_traces
            .push(astra_runtime::turn::context_assembly_trace::ContextAssemblyTrace {
                turn_id: "turn-3".into(),
                tools: astra_runtime::turn::context_assembly_trace::ToolSelectionTrace {
                    selection_strategy: "code-intel".into(),
                    selection_confidence: 0.92,
                    tools_selected: vec![astra_runtime::turn::context_assembly_trace::ToolSelected {
                        tool_name: "lsp".into(),
                        score: 1.0,
                        tokens: 0,
                        selection_factors: Vec::new(),
                    }],
                    ..Default::default()
                },
                memory: astra_runtime::turn::context_assembly_trace::MemoryRetrievalTrace {
                    query: "resume trace persistence".into(),
                    memories_selected: vec![astra_runtime::turn::context_assembly_trace::MemorySelection {
                        memory_id: "m1".into(),
                        memory_type: "semantic".into(),
                        content_preview: "trace".into(),
                        relevance_score: 0.8,
                        tokens: 10,
                        source: astra_runtime::turn::context_assembly_trace::MemorySource::Memoria,
                    }],
                    ..Default::default()
                },
                history: astra_runtime::turn::context_assembly_trace::HistorySelectionTrace {
                    turns_compressed: vec![astra_runtime::turn::context_assembly_trace::TurnCompression {
                        turn_index: 1,
                        role: "assistant".into(),
                        original_tokens: 100,
                        compressed_tokens: 50,
                        compression_method:
                            astra_runtime::turn::context_assembly_trace::CompressionMethod::ReactiveCompact,
                        information_lost: Vec::new(),
                    }],
                    compression_ratio: 0.5,
                    tokens_before: 100,
                    tokens_after: 50,
                    ..Default::default()
                },
                token_budget: astra_runtime::turn::context_assembly_trace::TokenBudgetTrace {
                    max_tokens: 16_000,
                    total_used: 8_200,
                    budget_pressure: 0.76,
                    ..Default::default()
                },
                explanations: vec![astra_runtime::turn::context_assembly_trace::DecisionExplanation {
                    decision_type:
                        astra_runtime::turn::context_assembly_trace::DecisionType::StrategyChoice {
                            strategy: "code-intel".into(),
                        },
                    reasoning: "Need symbol-aware context.".into(),
                    alternatives_considered: Vec::new(),
                    confidence: 0.9,
                }],
                ..Default::default()
            });
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-trace", "m");
        sync_context_trace_to_workspace(&state, &mut ws);

        let trace = ws.last_context_trace.expect("missing trace summary");
        assert_eq!(trace.turn_id, "turn-3");
        assert_eq!(
            trace
                .tool_selection
                .as_ref()
                .map(|selection| selection.selected_tools.clone()),
            Some(vec!["lsp".to_string()])
        );
        assert_eq!(
            trace
                .memory
                .as_ref()
                .map(|memory| memory.selected_memory_ids.len()),
            Some(1)
        );
        assert_eq!(
            trace.budget.as_ref().map(|budget| budget.total_used),
            Some(8_200)
        );
    }

    #[test]
    fn initialize_journal_preserves_existing_workspace() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = "sess-existing-workspace";
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(sid, "old-model");
        ws.turn_count = 7;
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_selection: Some(ContextTraceToolSelection {
                tools_available: 8,
                selected_tools: vec!["lsp".into()],
                rejected_tools: 2,
                strategy: "code-intel".into(),
                confidence: 0.9,
                latency_ms: 11,
            }),
            memory: None,
            history: None,
            budget: Some(ContextTraceBudgetSignal {
                max_tokens: 4096,
                total_used: 700,
                budget_pressure: 0.17,
                compression_triggered: false,
            }),
            timing: None,
            explanations: Vec::new(),
        });
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = ReplState::default();
        state.model = Some("new-model".into());
        initialize_journal(&mut state, sid);

        let persisted = astra_services::session_workspace::read_workspace(sid).unwrap();
        assert_eq!(persisted.model, "old-model");
        assert_eq!(persisted.turn_count, 7);
        assert_eq!(
            persisted
                .last_context_trace
                .as_ref()
                .map(|trace| trace.turn_id.as_str()),
            Some("turn-7")
        );
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
    fn create_manual_repl_checkpoint_returns_compact_summary() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = uuid::Uuid::new_v4().to_string();
        let journal = session_journal::JournalWriter::new(&sid).unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "test-model");
        ws.turn_count = 3;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = ReplState {
            session_id: Some(sid.clone()),
            journal: Some(journal),
            model: Some("test-model".to_string()),
            ..Default::default()
        };
        state.history.push(("hi".into(), "hello".into()));

        let summary = create_manual_repl_checkpoint(&mut state, "").unwrap();
        assert_eq!(summary.headline(), "Checkpoint #1 saved (turn 3)");
        assert!(summary.checkpoint_path.exists());
        assert!(summary.heavy_path.exists());
        assert!(!summary.cloud_sync_queued);
    }

    #[test]
    fn short_continuation_prompt_is_detected() {
        // Original keywords
        assert!(is_short_continuation_prompt("继续"));
        assert!(is_short_continuation_prompt("continue"));
        assert!(is_short_continuation_prompt("resume"));
        // Expanded keywords
        assert!(is_short_continuation_prompt("go ahead"));
        assert!(is_short_continuation_prompt("do it"));
        assert!(is_short_continuation_prompt("fix it"));
        assert!(is_short_continuation_prompt("yes"));
        assert!(is_short_continuation_prompt("ok"));
        assert!(is_short_continuation_prompt("sure"));
        assert!(is_short_continuation_prompt("proceed"));
        assert!(is_short_continuation_prompt("next"));
        assert!(is_short_continuation_prompt("keep going"));
        assert!(is_short_continuation_prompt("好的"));
        assert!(is_short_continuation_prompt("可以"));
        assert!(is_short_continuation_prompt("是的"));
        assert!(is_short_continuation_prompt("行"));
        // Still rejected
        assert!(!is_short_continuation_prompt(
            "继续修这个 bug，并顺便看下另一个问题"
        ));
        assert!(!is_short_continuation_prompt("fix this bug"));
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

    #[test]
    fn build_effective_line_injects_session_goal_with_anchor() {
        let state = ReplState {
            continuation_anchor: Some(
                "Latest user task: fix auth middleware\nLatest assistant direction: add JWT validation"
                    .to_string(),
            ),
            session_goal: Some("build a REST API with axum".to_string()),
            ..ReplState::default()
        };

        let effective = build_effective_line("ok", &state);
        assert!(
            effective.contains("[Continuation anchor]"),
            "anchor injected"
        );
        assert!(
            effective.contains("Session goal: build a REST API with axum"),
            "session goal present"
        );
        assert!(
            effective.contains("fix auth middleware"),
            "latest task present"
        );

        // Without session_goal
        let state_no_goal = ReplState {
            continuation_anchor: Some("Latest user task: fix auth".to_string()),
            session_goal: None,
            ..ReplState::default()
        };
        let effective_no_goal = build_effective_line("sure", &state_no_goal);
        assert!(effective_no_goal.contains("[Continuation anchor]"));
        assert!(!effective_no_goal.contains("Session goal:"));
    }

    #[test]
    fn build_effective_line_skill_dev_reads_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\n---\n# Test\nDo stuff.",
        )
        .unwrap();

        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "test-skill".to_string(),
                dir: skill_dir,
            }),
            ..ReplState::default()
        };

        let effective = build_effective_line("improve this skill", &state);
        assert!(effective.contains("[SKILL DEV: test-skill]"));
        assert!(effective.contains("Do stuff."));
        assert!(effective.contains("improve this skill"));
    }

    #[test]
    fn build_effective_line_skill_dev_picks_up_external_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("evolving");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: evolving\n---\nV1").unwrap();

        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "evolving".to_string(),
                dir: skill_dir.clone(),
            }),
            ..ReplState::default()
        };

        let turn1 = build_effective_line("check", &state);
        assert!(turn1.contains("V1"));

        // Simulate external edit between turns
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: evolving\n---\nV2 rewritten",
        )
        .unwrap();

        let turn2 = build_effective_line("check again", &state);
        assert!(!turn2.contains("V1"), "should not contain old content");
        assert!(turn2.contains("V2 rewritten"), "should contain new content");
    }

    #[test]
    fn build_effective_line_skill_dev_missing_file_falls_through() {
        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "ghost".to_string(),
                dir: std::path::PathBuf::from("/nonexistent/path/ghost"),
            }),
            ..ReplState::default()
        };

        let effective = build_effective_line("hello", &state);
        assert_eq!(effective, "hello");
    }

    #[test]
    fn build_effective_line_skill_dev_empty_file_falls_through() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("empty-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "").unwrap();

        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "empty-skill".to_string(),
                dir: skill_dir,
            }),
            ..ReplState::default()
        };

        let effective = build_effective_line("hello", &state);
        // Empty SKILL.md should not inject a useless prefix
        assert_eq!(effective, "hello");
    }

    #[test]
    fn build_effective_line_skill_dev_shows_actual_path() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("custom-loc");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: custom-loc\n---\nBody",
        )
        .unwrap();

        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "custom-loc".to_string(),
                dir: skill_dir.clone(),
            }),
            ..ReplState::default()
        };

        let effective = build_effective_line("x", &state);
        // Must contain the actual path, not a hardcoded .astra/skills/ path
        let expected_path = skill_dir.join("SKILL.md").display().to_string();
        assert!(
            effective.contains(&expected_path),
            "should contain actual path: {expected_path}"
        );
    }

    #[test]
    fn build_effective_line_skill_dev_combines_with_system_skills_and_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("combo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: combo\n---\nCombo skill",
        )
        .unwrap();

        let state = ReplState {
            skill_dev: Some(super::super::SkillDevState {
                name: "combo".to_string(),
                dir: skill_dir,
            }),
            active_system_skills: vec![prompts::builtin_concise_skill()],
            continuation_anchor: Some("Previous task: fix auth".to_string()),
            ..ReplState::default()
        };

        // Short continuation prompt triggers all three layers
        let effective = build_effective_line("continue", &state);
        assert!(effective.contains("[SKILL DEV: combo]"), "skill dev prefix");
        assert!(effective.contains("Concise"), "system skill");
        assert!(effective.contains("[Continuation anchor]"), "anchor");
        assert!(effective.contains("fix auth"), "anchor content");
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
        let cache_pct = cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_zero_when_no_cache() {
        let prompt = 1000u64;
        let cache_read = 0u64;
        let cache_pct = cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_100_when_all_cached() {
        let prompt = 0u64;
        let cache_read = 5000u64;
        let cache_pct = cache_read as f64 / (prompt + cache_read).max(1) as f64 * 100.0;
        assert!((cache_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn session_cost_accumulation() {
        let mut state = ReplState::default();
        state.cached_pricing = astra_services::models::PricingData {
            prompt: 3.0,             // $3/1K prompt tokens
            completion: 15.0,        // $15/1K completion tokens
            cache_read: Some(0.3),   // $0.3/1K (10% of prompt)
            cache_write: Some(3.75), // $3.75/1K (125% of prompt)
        };

        // First turn
        let cost1 = crate::slash_stats::cost_for_tokens(1000, 500, 800, 100, &state.cached_pricing);
        state.total_session_cost += cost1;
        assert!(cost1 > 0.0);

        // Second turn
        let cost2 = crate::slash_stats::cost_for_tokens(2000, 1000, 1500, 0, &state.cached_pricing);
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

    // ── Context Continuity Integration Tests ──────────────────────────

    /// Simulates compaction by manually building the post-compaction history,
    /// then verifies that continuation anchor still triggers on short prompts.
    #[test]
    fn continuation_anchor_survives_simulated_compaction() {
        // Phase 1: Build a 10-turn history
        let mut history: Vec<(String, String)> = (1..=10)
            .map(|i| (format!("user msg {i}"), format!("assistant reply {i}")))
            .collect();

        // Phase 2: Simulate compaction — keep last 3 turns, summarize rest
        let keep = 3;
        let trimmed = history.len().saturating_sub(keep);
        let summary = "User explored Rust async patterns, asked about pinning, \
                        debugged a lifetime issue, and reviewed tokio spawn.";
        let anchor_text =
            "- [fact] Rust Pin<T> prevents moves\n- [fact] tokio::spawn requires 'static";
        let compact_msg = compact_assistant_message(trimmed, summary, Some(anchor_text));
        let summary_entry = (String::new(), compact_msg);

        let mut new_history = vec![summary_entry];
        new_history.extend_from_slice(&history[trimmed..]);
        history = new_history;

        // Phase 3: Verify post-compaction structure
        assert_eq!(history.len(), 4); // 1 summary + 3 kept
        assert!(history[0].0.is_empty(), "compacted entry has empty user");
        assert!(history[0].1.contains("[Prior context — 7 turns compacted]"));
        assert!(history[0].1.contains("[Session memory anchor]"));
        assert!(history[0].1.contains("Rust Pin<T>"));

        // Phase 4: Verify continuation anchor still works on short prompt
        let state = ReplState {
            continuation_anchor: Some(
                "Latest user task: debug lifetime in tokio::spawn\n\
                 Latest assistant direction: add 'static bound to closure"
                    .to_string(),
            ),
            history,
            ..ReplState::default()
        };

        let effective = build_effective_line("继续", &state);
        assert!(
            effective.contains("[Continuation anchor]"),
            "anchor injection must work after compaction"
        );
        assert!(
            effective.contains("debug lifetime"),
            "anchor content preserved"
        );
        assert!(
            effective.contains("[User follow-up]\n继续"),
            "user prompt appended"
        );

        // Phase 5: Normal prompt should NOT inject anchor
        let normal = build_effective_line("explain Pin in detail", &state);
        assert!(
            !normal.contains("[Continuation anchor]"),
            "normal prompt must not inject anchor"
        );
    }

    /// Verifies history_as_messages produces correct OpenAI message sequence
    /// after compaction: compacted entry → kept turns → new user message.
    #[test]
    fn history_as_messages_post_compaction_preserves_order() {
        // Simulate 8-turn conversation compacted to summary + 3 recent
        let summary = compact_assistant_message(
            5,
            "User built a REST API with axum, added auth middleware.",
            Some("- [fact] axum uses tower layers"),
        );
        let history: Vec<(String, String)> = vec![
            (String::new(), summary),                                   // compacted
            ("add rate limiting".into(), "use tower RateLimit".into()), // turn 6
            ("show example".into(), "```rust\nuse tower...```".into()), // turn 7
            ("deploy it".into(), "docker build...".into()),             // turn 8
        ];

        let messages = history_as_messages(&history);

        // Compacted entry: only assistant (no user role)
        assert_eq!(messages[0]["role"], "assistant");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("5 turns compacted")
        );
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("[Session memory anchor]")
        );

        // Recent turns: alternating user/assistant
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "add rate limiting");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "use tower RateLimit");

        // Total: 1 (compact) + 3×2 (kept turns) = 7
        assert_eq!(messages.len(), 7);
    }

    fn stub_stream_result(full_text: &str) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            full_text: full_text.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            memoria_ms: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
        }
    }

    fn test_selector() -> tool_selector::TfIdfSelector {
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        tool_selector::TfIdfSelector::new(registry)
    }

    fn tool_call_record(
        name: &str,
        ok: bool,
        result_preview: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: result_preview.map(str::to_string),
            file_path: None,
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_sets_prompt_hint_for_followup() {
        let (_tmp, _g) = isolated_sessions_dir();
        super::repl_ui::clear_followup_prompt_hint();

        let selector = test_selector();
        let mut state = ReplState::default();
        let mut result = stub_stream_result("Updated the code.");
        result.tools_used = vec!["str_replace".to_string()];

        apply_turn_success(
            &mut state,
            &selector,
            None,
            "fix the bug",
            result,
            Instant::now(),
        );

        assert_eq!(
            state
                .pending_followup_suggestion
                .as_ref()
                .map(|suggestion| suggestion.text.as_str()),
            Some("run the tests")
        );
        assert_eq!(
            super::repl_ui::prompt_inline_hint(""),
            Some("run the tests".to_string())
        );

        super::repl_ui::clear_followup_prompt_hint();
    }

    #[test]
    fn report_turn_failure_persists_filtered_partial_metrics() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-turn-failure-{}", uuid::Uuid::new_v4());
        let mut state = ReplState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let failure = crate::TurnFailure {
            error: "model overloaded".into(),
            partial: crate::PartialTurnData {
                tool_call_records: vec![
                    tool_call_record(
                        "bash",
                        false,
                        Some("Skipped: the skill already completed this work."),
                    ),
                    tool_call_record("read_file", true, Some("contents")),
                ],
                tools_used: vec!["read_file".into()],
                prompt_tokens: 13,
                completion_tokens: 7,
                tool_calls_count: 1,
                partial_text: "Partial analysis".into(),
                ..Default::default()
            },
        };

        report_turn_failure(
            &mut state,
            None,
            "show session metrics",
            &failure,
            Instant::now(),
        );

        let event = state.last_turn_event.as_ref().expect("turn_error event");
        assert_eq!(event.tool_count, Some(1));
        assert_eq!(event.tools_used, Some(vec!["read_file".into()]));
        assert_eq!(event.tokens_in, Some(13));
        assert_eq!(event.tokens_out, Some(7));
        assert_eq!(event.tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.last_response.as_deref(), Some("Partial analysis"));

        let events = session_journal::read_journal(&sid).unwrap();
        let persisted = events.last().expect("persisted turn_error");
        assert_eq!(persisted.tool_count, Some(1));
        assert_eq!(persisted.tools_used, Some(vec!["read_file".into()]));
        assert_eq!(persisted.tool_calls.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn commit_turn_persists_turn_evaluation_event() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-turn-eval-{}", uuid::Uuid::new_v4());
        let mut state = ReplState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            recent_tools: vec!["git_status".into()],
            ..Default::default()
        };
        let mut result = stub_stream_result("Workspace is clean.");
        result.tools_used = vec!["git_status".into()];
        result.tool_calls_count = 1;
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git_status".into(),
            ok: true,
            ms: 12,
            error: None,
            input_bytes: Some(16),
            output_bytes: Some(240),
            args_preview: None,
            result_preview: Some("clean".into()),
            file_path: None,
        }];

        let learning =
            analyze_repl_turn_learning("git status", state.turn, &state.recent_tools, &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "git status",
            &result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::TurnEvaluation)
            .expect("turn evaluation event");
        assert_eq!(event.turn, Some(1));
        let metadata = event.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["success"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    #[serial_test::serial]
    fn apply_turn_success_clears_stale_prompt_hint_when_suppressed() {
        let (_tmp, _g) = isolated_sessions_dir();
        super::repl_ui::set_followup_prompt_hint(Some("stale hint".to_string()));

        let selector = test_selector();
        let mut state = ReplState {
            plan_mode: Some(plan_decompose::PlanModeState::new(
                "goal".to_string(),
                plan_decompose::ProjectContext::default(),
            )),
            ..Default::default()
        };
        let result = stub_stream_result("Plan updated.");

        apply_turn_success(
            &mut state,
            &selector,
            None,
            "continue",
            result,
            Instant::now(),
        );

        assert!(state.pending_followup_suggestion.is_none());
        assert_eq!(super::repl_ui::prompt_inline_hint(""), None);

        super::repl_ui::clear_followup_prompt_hint();
    }

    /// Verifies build_continuation_anchor truncates long content to 220 chars
    /// and formats correctly for both user and assistant parts.
    #[test]
    fn continuation_anchor_builder_truncates_long_content() {
        let long_user_input = "a".repeat(300);
        let long_assistant = format!("{}\nSecond line of detail", "b".repeat(300));

        let state = ReplState::default();
        let result = stub_stream_result(&long_assistant);

        let anchor = build_continuation_anchor(&state, &long_user_input, &result);
        let anchor = anchor.expect("should produce anchor");

        // User part truncated to 220 chars
        assert!(anchor.contains("Latest user task: "));
        let user_part = anchor
            .split("Latest user task: ")
            .nth(1)
            .unwrap()
            .split('\n')
            .next()
            .unwrap();
        assert_eq!(user_part.chars().count(), 220);

        // Assistant part truncated to 220 chars (first non-empty line)
        assert!(anchor.contains("Latest assistant direction: "));
        let assistant_part = anchor.split("Latest assistant direction: ").nth(1).unwrap();
        assert_eq!(assistant_part.chars().count(), 220);
    }

    /// Verifies that when user input is empty, the previous anchor is preserved.
    #[test]
    fn continuation_anchor_preserves_on_empty_input() {
        let state = ReplState {
            continuation_anchor: Some("Previous anchor content".to_string()),
            ..ReplState::default()
        };
        let result = stub_stream_result("new response");

        let anchor = build_continuation_anchor(&state, "", &result);
        assert_eq!(anchor.as_deref(), Some("Previous anchor content"));
    }

    /// Simulates a multi-turn error recovery scenario:
    /// Turn 1 succeeds, Turn 2 fails (not added to history), Turn 3 retries.
    /// Verifies history integrity after failed turn is excluded.
    #[test]
    fn failed_turn_excluded_from_history_preserves_continuity() {
        let mut state = ReplState::default();

        // Turn 1: success
        state.history.push((
            "explain ownership".into(),
            "Ownership in Rust means each value has exactly one owner...".into(),
        ));
        state.turn = 1;
        state.continuation_anchor = Some(
            "Latest user task: explain ownership\nLatest assistant direction: Ownership in Rust means each value has exactl"
                .to_string(),
        );

        // Turn 2: fails — simulate by NOT adding to history
        // (production code: handle_chat_input returns Err, history unchanged)
        let _failed_user_msg = "now explain borrowing";
        // state.history.push(...) is NOT called — turn failed
        // state.turn stays at 1

        // Turn 3: user retries
        state.history.push((
            "now explain borrowing".into(),
            "Borrowing lets you reference data without taking ownership...".into(),
        ));
        state.turn = 2;

        // Verify: history has exactly 2 successful turns
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].0, "explain ownership");
        assert_eq!(state.history[1].0, "now explain borrowing");

        // Verify: messages for API call are coherent
        let messages = history_as_messages(&state.history);
        assert_eq!(messages.len(), 4); // u1, a1, u2, a2
        assert_eq!(messages[0]["content"], "explain ownership");
        assert_eq!(messages[2]["content"], "now explain borrowing");

        // Verify: continuation still works after recovery
        state.continuation_anchor = Some(
            "Latest user task: now explain borrowing\nLatest assistant direction: Borrowing lets you reference data"
                .to_string(),
        );
        let effective = build_effective_line("continue", &state);
        assert!(effective.contains("[Continuation anchor]"));
        assert!(effective.contains("explain borrowing"));

        // Verify: the failed message is nowhere in the conversation
        let _all_content: String = messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        // The failed turn's content DOES appear in Turn 3 (retry same message)
        // but there should be exactly 2 user messages, not 3
        let user_messages: Vec<_> = messages.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(
            user_messages.len(),
            2,
            "failed turn must not create extra user message"
        );
    }

    // ── Relevance scoring tests ──────────────────────────────────────────

    #[test]
    fn tokenize_for_relevance_basic() {
        let tokens = tokenize_for_relevance("Hello World, this is a TEST!");
        assert!(tokens.contains("hello"));
        assert!(tokens.contains("world"));
        assert!(tokens.contains("this"));
        assert!(tokens.contains("test"));
        // Single-char tokens filtered out
        assert!(!tokens.contains("a"));
    }

    #[test]
    fn tokenize_for_relevance_empty() {
        let tokens = tokenize_for_relevance("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn score_turn_relevance_exact_overlap() {
        let context_tokens = tokenize_for_relevance("database migration schema");
        let turn = (
            "How to run database migration?".to_string(),
            "Use the migration tool.".to_string(),
        );
        let score = score_turn_relevance(&turn, &context_tokens);
        assert!(
            score > 0.0,
            "overlapping turn should score > 0, got {score}"
        );
    }

    #[test]
    fn score_turn_relevance_no_overlap() {
        let context_tokens = tokenize_for_relevance("database migration schema");
        let turn = (
            "What color is the sky?".to_string(),
            "It is blue.".to_string(),
        );
        let score = score_turn_relevance(&turn, &context_tokens);
        assert!(
            score == 0.0,
            "non-overlapping turn should score 0, got {score}"
        );
    }

    #[test]
    fn score_turn_relevance_user_weighted_higher() {
        let context_tokens = tokenize_for_relevance("deploy kubernetes cluster");
        // User message has overlap, assistant doesn't
        let turn_user_match = (
            "deploy to kubernetes".to_string(),
            "Sure, I will help.".to_string(),
        );
        // Assistant message has overlap, user doesn't
        let turn_asst_match = (
            "Please help me.".to_string(),
            "deploy to kubernetes cluster".to_string(),
        );
        let score_user = score_turn_relevance(&turn_user_match, &context_tokens);
        let score_asst = score_turn_relevance(&turn_asst_match, &context_tokens);
        assert!(
            score_user > score_asst,
            "user-side match ({score_user}) should score higher than assistant-side ({score_asst})"
        );
    }

    #[test]
    fn select_turns_preserves_recent_and_relevant() {
        // 10 turns: turns 0,1 are about "database", turns 2-7 are filler, 8-9 are recent
        let history: Vec<(String, String)> = vec![
            (
                "setup database schema".into(),
                "Done, schema created.".into(),
            ),
            (
                "add database indexes".into(),
                "Added indexes on user_id.".into(),
            ),
            ("what is the weather".into(), "It's sunny.".into()),
            ("tell me a joke".into(), "Why did the chicken...".into()),
            ("random topic alpha".into(), "Alpha response.".into()),
            ("random topic beta".into(), "Beta response.".into()),
            ("random topic gamma".into(), "Gamma response.".into()),
            ("random topic delta".into(), "Delta response.".into()),
            ("recent turn one".into(), "Recent reply one.".into()),
            ("recent turn two".into(), "Recent reply two.".into()),
        ];
        let kept = select_turns_for_compaction(&history, 6, "database query optimization");
        // Should keep at least turns 8,9 (recent) and turns 0,1 (relevant to "database")
        assert!(
            kept.contains(&8) && kept.contains(&9),
            "recent turns 8,9 must be kept: {kept:?}"
        );
        assert!(
            kept.contains(&0) || kept.contains(&1),
            "relevant database turns 0/1 should be kept: {kept:?}"
        );
        // Should NOT keep all filler turns
        let filler_kept: Vec<usize> = kept
            .iter()
            .filter(|&&i| (2..8).contains(&i))
            .copied()
            .collect();
        assert!(
            filler_kept.len() < 6,
            "not all filler turns should be kept: {kept:?}"
        );
        assert!(
            kept.len() <= 6,
            "total kept should not exceed budget: {kept:?}"
        );
    }

    #[test]
    fn select_turns_small_history_keeps_all() {
        let history: Vec<(String, String)> = vec![
            ("hello".into(), "world".into()),
            ("foo".into(), "bar".into()),
        ];
        let kept = select_turns_for_compaction(&history, 6, "anything");
        assert_eq!(kept.len(), 2, "should keep all when history < budget");
    }

    #[test]
    fn select_turns_budget_equals_total_keeps_all() {
        let history: Vec<(String, String)> =
            (0..6).map(|i| (format!("q{i}"), format!("a{i}"))).collect();
        let kept = select_turns_for_compaction(&history, 6, "anything");
        assert_eq!(kept.len(), 6, "budget == total should keep all");
    }

    #[test]
    fn select_turns_budget_one_keeps_last() {
        let history: Vec<(String, String)> = vec![
            ("old question".into(), "old answer".into()),
            ("recent question".into(), "recent answer".into()),
            ("latest question".into(), "latest answer".into()),
        ];
        let kept = select_turns_for_compaction(&history, 1, "irrelevant");
        assert_eq!(kept.len(), 1, "budget=1 should keep exactly 1 turn");
        assert_eq!(*kept.last().unwrap(), 2, "should keep the very last turn");
    }

    #[test]
    fn select_turns_all_zero_scores_only_recent() {
        // No overlap at all between context and older turns
        let history: Vec<(String, String)> = vec![
            ("alpha beta gamma".into(), "one two three".into()),
            ("delta epsilon zeta".into(), "four five six".into()),
            ("eta theta iota".into(), "seven eight nine".into()),
            ("recent turn A".into(), "response A".into()),
            ("recent turn B".into(), "response B".into()),
        ];
        let kept = select_turns_for_compaction(&history, 4, "completely unrelated xylophone");
        // Should keep 2 recent (budget/2) + 0 relevant older = 2 recent minimum
        // But min_recent = max(4/2, 2) = 2, so keeps turns 3,4
        // relevance_slots = 2, but all scores are 0, so none added
        assert!(
            kept.len() <= 4,
            "should respect budget even with zero scores: {kept:?}"
        );
        assert!(
            kept.contains(&3) && kept.contains(&4),
            "must keep the most recent turns: {kept:?}"
        );
    }

    #[test]
    fn select_turns_tied_scores_deterministic() {
        // Two older turns with identical overlap
        let history: Vec<(String, String)> = vec![
            ("deploy kubernetes".into(), "done".into()),
            ("deploy kubernetes".into(), "done again".into()),
            ("filler unrelated".into(), "filler response".into()),
            ("recent turn".into(), "recent response".into()),
        ];
        let kept1 = select_turns_for_compaction(&history, 3, "deploy kubernetes cluster");
        let kept2 = select_turns_for_compaction(&history, 3, "deploy kubernetes cluster");
        assert_eq!(
            kept1, kept2,
            "tied scores should produce deterministic results"
        );
    }

    #[test]
    fn tokenize_for_relevance_cjk_characters() {
        // CJK characters are alphanumeric, should be tokenized
        let tokens = tokenize_for_relevance("数据库 迁移 schema");
        assert!(tokens.contains("数据库"), "should tokenize CJK words");
        assert!(tokens.contains("迁移"));
        assert!(tokens.contains("schema"));
    }

    #[test]
    fn tokenize_for_relevance_hyphenated_words() {
        let tokens = tokenize_for_relevance("auto-approve real-time");
        // Hyphens are kept in tokenizer (not split on)
        assert!(tokens.contains("auto-approve") || tokens.contains("auto"));
    }

    #[test]
    fn score_turn_relevance_empty_query_returns_zero() {
        let empty_tokens = tokenize_for_relevance("");
        let turn = ("some content here".into(), "and more content".into());
        let score = score_turn_relevance(&turn, &empty_tokens);
        assert_eq!(score, 0.0, "empty query should score 0");
    }

    #[test]
    fn select_turns_empty_context_still_works() {
        let history: Vec<(String, String)> = (0..10)
            .map(|i| (format!("question {i}"), format!("answer {i}")))
            .collect();
        let kept = select_turns_for_compaction(&history, 4, "");
        // With empty context, only recent turns kept
        assert!(kept.len() <= 4);
        assert!(kept.contains(&9), "must keep latest turn");
    }

    #[test]
    fn classify_turn_error_timeout() {
        use astra_core::ErrorKind;
        // "Request timed out after 30s" → ToolTimeout (local command timeout pattern)
        assert_eq!(
            classify_turn_error("Request timed out after 30s"),
            ErrorKind::ToolTimeout
        );
        // "connection timeout" → Network (contains "connection")
        assert_eq!(
            classify_turn_error("connection timeout"),
            ErrorKind::Network
        );
        // "TIMEOUT waiting for LLM" → Network (generic timeout)
        assert_eq!(
            classify_turn_error("TIMEOUT waiting for LLM"),
            ErrorKind::Network
        );
    }

    #[test]
    fn classify_turn_error_rate_limit() {
        use astra_core::ErrorKind;
        // "Rate limit" → Network (transient, contains "rate limit")
        assert_eq!(
            classify_turn_error("Rate limit exceeded"),
            ErrorKind::Network
        );
        assert_eq!(
            classify_turn_error("HTTP 429: too many requests"),
            ErrorKind::Network
        );
    }

    #[test]
    fn classify_turn_error_network() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_turn_error("Connection refused"),
            ErrorKind::Network
        );
        assert_eq!(
            classify_turn_error("DNS resolution failed"),
            ErrorKind::Network
        );
        assert_eq!(
            classify_turn_error("network unreachable"),
            ErrorKind::Network
        );
    }

    #[test]
    fn classify_turn_error_auth() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_turn_error("HTTP 401 Unauthorized"),
            ErrorKind::Auth
        );
        assert_eq!(classify_turn_error("403 Forbidden"), ErrorKind::Auth);
        assert_eq!(
            classify_turn_error("Authentication failed"),
            ErrorKind::Auth
        );
    }

    #[test]
    fn classify_turn_error_server() {
        use astra_core::ErrorKind;
        // 500/502/503 → Network (transient)
        assert_eq!(
            classify_turn_error("Internal Server Error 500"),
            ErrorKind::Network
        );
        assert_eq!(classify_turn_error("502 Bad Gateway"), ErrorKind::Network);
        assert_eq!(
            classify_turn_error("503 Service Unavailable"),
            ErrorKind::Network
        );
    }

    #[test]
    fn classify_turn_error_unknown() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_turn_error("something weird happened"),
            ErrorKind::Unknown
        );
        assert_eq!(classify_turn_error(""), ErrorKind::Unknown);
    }

    #[test]
    fn classify_turn_error_priority_timeout_over_network() {
        use astra_core::ErrorKind;
        // "connection timeout" → Network (both patterns match, Network wins)
        assert_eq!(
            classify_turn_error("connection timeout"),
            ErrorKind::Network
        );
    }
}
