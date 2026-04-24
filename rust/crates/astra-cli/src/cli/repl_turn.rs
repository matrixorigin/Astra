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

/// Map a runtime stall event's `stall_type` string to the journal confidence.
///
/// - `sig_stall` (exact-repetition signature stall): confidence 1.0.
/// - `skill_lockout[:<name>]` (skill re-entry hard lockout, reentry ≥ 3):
///   confidence 1.0 — the runtime has already blocked the call, so the
///   signal is deterministic.
/// - Anything else: 0.0 (heuristic; emission is skipped to avoid polluting
///   downstream reflection / auto-tuning pipelines with no-op stalls).
fn stall_type_confidence(stall_type: &str) -> f64 {
    match stall_type {
        "sig_stall" => 1.0,
        s if s == "skill_lockout" || s.starts_with("skill_lockout:") => 1.0,
        _ => 0.0,
    }
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

/// Emit an `EvolutionSignal::UserCorrection` (if evolution service is wired)
/// from the current conversation context. Extracted from `run_chat_turn` for
/// unit-testability; production code path is unchanged.
async fn emit_user_correction_signal(state: &ReplState, correction_text: &str) {
    let Some(evo) = state.evolution_service.as_ref() else {
        return;
    };
    let prior_assistant_text = state
        .history
        .last()
        .map(|(_u, a)| a.clone())
        .unwrap_or_default();
    let skill_context = state.recent_tools.last().cloned();
    let turn_id = format!("turn-{}", state.turn);
    evo.add_signal(
        astra_runtime::evolution::types::EvolutionSignal::UserCorrection {
            correction_text: correction_text.to_string(),
            prior_assistant_text,
            skill_context,
            turn_id,
        },
    )
    .await;
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
        if is_low_information_followup(&line) {
            if let Err(e) =
                slash_session::restore_session_into_state(&session_id, ctx.profile, ctx.api, state)
                    .await
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
    // P3.3 — pending plan-resume digest. Kept until the user sends an
    // explicit resume-like line, then consumed once.
    let plan_resume_digest = consume_plan_resume_if_matches(state, &line);
    let mut effective_line = build_effective_line(&line, state);
    effective_line = apply_resume_context(effective_line, resume_guidance, plan_resume_digest);
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
                apply_turn_success_async(
                    state,
                    ctx.selector,
                    ctx.profile,
                    &line,
                    result,
                    turn_start,
                )
                .await;
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
                                apply_turn_success_async(
                                    state,
                                    ctx.selector,
                                    ctx.profile,
                                    &line,
                                    result,
                                    turn_start,
                                )
                                .await;
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
                                        apply_turn_success_async(
                                            state,
                                            ctx.selector,
                                            ctx.profile,
                                            &line,
                                            result,
                                            turn_start,
                                        )
                                        .await;
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

pub(super) fn consume_plan_resume_if_matches(state: &mut ReplState, line: &str) -> Option<String> {
    astra_runtime::plan::plan_resume::message_signals_resume(line)
        .then(|| state.pending_plan_resume_digest.take())
        .flatten()
}

fn apply_resume_context(
    mut effective_line: String,
    resume_guidance: Option<String>,
    plan_resume_digest: Option<String>,
) -> String {
    if let Some(guidance) = resume_guidance {
        effective_line = format!("{guidance}\n\n{effective_line}");
    }
    if let Some(digest) = plan_resume_digest {
        effective_line = format!("@resume-plan\n{digest}\n\n{effective_line}");
    }
    effective_line
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
        .filter(|_| is_low_information_followup(line))
    {
        let goal_line = state
            .session_goal
            .as_deref()
            .map(|g| format!("Session goal: {g}\n"))
            .unwrap_or_default();
        effective_line = format!(
            "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
{goal_line}{anchor}\n\n[User follow-up]\n{effective_line}"
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

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn contains_any_token(haystack: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| haystack.contains(token))
}

fn is_low_information_followup(line: &str) -> bool {
    if is_short_continuation_prompt(line) {
        return true;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 32 {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_action = contains_any_token(
        &lower,
        &[
            "fix",
            "patch",
            "repair",
            "implement",
            "apply",
            "edit",
            "update",
            "test",
            "verify",
            "run",
            "commit",
            "push",
            "continue",
            "resume",
            "retry",
        ],
    ) || contains_any_token(
        trimmed,
        &[
            "修复",
            "修一下",
            "改一下",
            "改下",
            "处理一下",
            "处理下",
            "优化一下",
            "优化下",
            "测一下",
            "测试一下",
            "验证一下",
            "提交一下",
            "推一下",
            "继续",
            "重试",
        ],
    );
    if !has_action {
        return false;
    }

    let has_deictic_reference =
        contains_any_token(&lower, &["this", "it", "that", "them", "here", "there"])
            || contains_any_token(trimmed, &["这", "这个", "这里", "它", "这些", "那个"]);
    let has_question_shape =
        trimmed.ends_with('?') || trimmed.ends_with('？') || trimmed.ends_with('吗');
    let token_count = trimmed
        .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
        .filter(|part| !part.is_empty())
        .count();
    let short_ascii_action =
        (trimmed.is_ascii() || trimmed.contains(char::is_whitespace)) && token_count <= 3;

    has_deictic_reference || has_question_shape || short_ascii_action
}

fn summarize_assistant_for_anchor(full_text: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut total_chars = 0usize;

    for line in full_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if lines.len() >= 3 || total_chars >= 420 {
            break;
        }
        let clipped = truncate_chars(line, 160);
        total_chars += clipped.chars().count();
        lines.push(clipped);
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn summarize_anchor_artifacts(result: &StreamResult) -> Vec<String> {
    let mut lines = Vec::new();
    if !result.tools_used.is_empty() {
        lines.push(format!(
            "Recent tools: {}",
            result
                .tools_used
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    for call in result.tool_call_records.iter().take(3) {
        if let Some(preview) = call
            .args_preview
            .as_deref()
            .filter(|preview| !preview.trim().is_empty())
        {
            lines.push(format!(
                "Artifact: {} → {}",
                call.name,
                truncate_chars(preview.trim(), 120)
            ));
        }
    }

    lines
}

fn summarize_event_anchor_artifacts(event: Option<&session_journal::JournalEvent>) -> Vec<String> {
    let Some(event) = event else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    if let Some(tools_used) = event.tools_used.as_ref()
        && !tools_used.is_empty()
    {
        lines.push(format!(
            "Recent tools: {}",
            tools_used
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Some(tool_calls) = event.tool_calls.as_ref() {
        for call in tool_calls.iter().take(3) {
            if let Some(preview) = call
                .args_preview
                .as_deref()
                .filter(|preview| !preview.trim().is_empty())
            {
                lines.push(format!(
                    "Artifact: {} → {}",
                    call.name,
                    truncate_chars(preview.trim(), 120)
                ));
            }
        }
    }

    lines
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

    let user_summary = truncate_chars(user_line, 220);
    let mut sections = vec![format!("Latest user task: {user_summary}")];

    if let Some(assistant_summary) = summarize_assistant_for_anchor(&result.full_text) {
        sections.push(format!("Latest assistant summary:\n{assistant_summary}"));
    }

    let artifact_lines = summarize_anchor_artifacts(result);
    if !artifact_lines.is_empty() {
        sections.extend(artifact_lines);
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n"))
    }
}

pub(super) fn rebuild_continuation_anchor_from_state(state: &mut ReplState) {
    state.last_response = state.history.last().map(|(_, assistant)| assistant.clone());

    let Some((user_line, assistant_text)) = state.history.last() else {
        state.continuation_anchor = None;
        return;
    };
    if user_line.trim().is_empty() {
        state.continuation_anchor = None;
        return;
    }

    let user_summary = truncate_chars(user_line, 220);
    let mut sections = vec![format!("Latest user task: {user_summary}")];

    if let Some(assistant_summary) = summarize_assistant_for_anchor(assistant_text) {
        sections.push(format!("Latest assistant summary:\n{assistant_summary}"));
    }

    sections.extend(summarize_event_anchor_artifacts(
        state.last_turn_event.as_ref(),
    ));
    state.continuation_anchor = Some(sections.join("\n"));
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
        file_state: Some(state.file_state.clone()),
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

        // Also emit an EvolutionSignal::UserCorrection so the evolution
        // service / auto-reflection pipeline can learn from it. Previously
        // only drift_user_corrections was recorded and no signal was ever
        // produced for user corrections in the production path.
        emit_user_correction_signal(state, message).await;
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
            file_state: Some(state.file_state.clone()),
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
        // Flush turn observability events (llm_round, tool timing) before the turn summary.
        if !result.turn_observability_events.is_empty() {
            if let Err(e) = journal.append_bulk(&result.turn_observability_events) {
                astra_core::agent_warn!("journal", "failed to write observability events: {e}");
            }
        }

        let mut turn_event = session_journal::JournalEvent::turn(
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

        // Attach per-turn git snapshot for rewind/fork/sync lineage.
        {
            let git_root = state
                .session_id
                .as_deref()
                .and_then(|sid| astra_services::session_workspace::read_workspace(sid).ok())
                .and_then(|ws| ws.git_root);
            let (git_head, git_branch) = super::cli_utils::git_snapshot(git_root.as_deref());
            turn_event = turn_event.with_git_snapshot(git_head, git_branch);
        }

        // Add turn observability summary.
        turn_event.llm_rounds = result.llm_rounds;
        let tool_ms: u64 = result
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .map(|r| r.ms)
            .sum();
        turn_event.total_tool_ms = Some(tool_ms);
        if let Some(dur) = turn_event.duration_ms {
            turn_event.total_llm_ms = Some(dur.saturating_sub(tool_ms));
        }

        // Store for /turn command
        state.last_turn_event = Some(turn_event.clone());

        if let Err(e) = journal.append(&turn_event) {
            astra_core::agent_warn!("journal", "failed to write turn event: {e}");
        }
        enqueue_ingestion(state, &turn_event);

        // Emit deferred context_assembly_recorded — only on successful turn commit.
        if let Some((_internal_turn, trace_json)) = &result.pending_context_assembly_trace {
            let trace = trace_json.clone();
            // Use the REPL's user-visible turn number, not the internal agentic
            // loop counter that was stored in the trace.
            let assembly_event = session_journal::JournalEvent::context_assembly_recorded(
                state.session_id.as_deref(),
                state.turn,
                trace,
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
                    let cp_number = cp.number;
                    mc.spawn_session_sync_task(async move {
                        if let Err(e) = astra_services::session_restore::push_checkpoint_to_cloud(
                            &pool,
                            &sid_owned,
                            &user_id_owned,
                            &cp_clone,
                        )
                        .await
                        {
                            astra_core::agent_warn!(
                                "checkpoint_sync",
                                "failed to push checkpoint {cp_number} to cloud for session {sid_owned}: {e}"
                            );
                        }
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
                mc.spawn_session_sync_task(async move {
                    if let Err(e) = astra_services::session_restore::push_step_checkpoint_to_cloud(
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
                    .await
                    {
                        astra_core::agent_warn!(
                            "checkpoint_sync",
                            "failed to push step checkpoint {cp_number} to cloud for session {sid_owned}: {e}"
                        );
                    }
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
                mc.spawn_session_sync_task(async move {
                    if let Err(e) =
                        astra_services::session_restore::push_context_trace_signal_to_cloud(
                            &pool,
                            &sid_owned,
                            &user_id_owned,
                            &trace_signal,
                        )
                        .await
                    {
                        astra_core::agent_warn!(
                            "context_trace_sync",
                            "failed to push context trace signal to cloud for session {sid_owned}: {e}"
                        );
                    }
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
                let user_id = state
                    .ingestion_user_id
                    .as_deref()
                    .unwrap_or("anonymous")
                    .to_string();
                let plan_json = ws.executing_plan_json.clone();
                let goal = ws.plan_goal.clone();
                let config = ws.plan_config_json.clone();
                let rounds = ws.plan_execution_rounds;
                let git_branch = ws.git_branch.clone();
                let model = if ws.model.is_empty() {
                    None
                } else {
                    Some(ws.model.clone())
                };
                mc.spawn_session_sync_task(async move {
                    if let Err(e) = astra_services::session_restore::push_session_state_to_cloud(
                        &pool,
                        &sid_owned,
                        &user_id,
                        plan_json.as_deref(),
                        goal.as_deref(),
                        config.as_deref(),
                        rounds,
                        git_branch.as_deref(),
                        model.as_deref(),
                    )
                    .await
                    {
                        astra_core::agent_warn!(
                            "session_state_sync",
                            "failed to push session state to cloud for session {sid_owned}: {e}"
                        );
                    }
                });
            }

            if let Err(e) = astra_services::session_workspace::write_workspace(&ws) {
                eprintln!("  ⚠ workspace write failed after stall detection: {e}");
            }
        }

        // Log stall events to journal (use state.turn for user turn, not internal loop turn)
        for (stall_type, _) in &result.stall_events {
            let confidence = stall_type_confidence(stall_type);
            if confidence == 0.0 {
                continue;
            }
            let stall_event = session_journal::JournalEvent::stall_detected(
                state.session_id.as_deref(),
                state.turn,
                stall_type,
                0, // nudge_count not tracked per-event; stall_type conveys severity
                confidence,
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

/// Test-only sync variant of `apply_turn_success`. Production code paths must
/// use [`apply_turn_success_async`] so the LLM-driven skill-improvement path
/// can await its network call. This wrapper keeps the existing synchronous
/// test fixtures working without pulling a tokio runtime into every assertion.
#[cfg(test)]
fn apply_turn_success(
    state: &mut ReplState,
    selector: &dyn tool_selector::ToolSelector,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    apply_turn_success_sync(state, selector, profile, line, result, turn_start);
    check_skill_improvement_inner(state);
}

async fn apply_turn_success_async(
    state: &mut ReplState,
    selector: &dyn tool_selector::ToolSelector,
    profile: Option<&str>,
    line: &str,
    result: StreamResult,
    turn_start: Instant,
) {
    apply_turn_success_sync(state, selector, profile, line, result, turn_start);
    check_skill_improvement_async(state).await;
}

fn apply_turn_success_sync(
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

    // Prefetch indicator
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
/// Minimal async-capable chat completion abstraction so the skill-improvement
/// LLM path can be unit-tested without real HTTP.
#[async_trait::async_trait]
pub(crate) trait SkillImproveLlm: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String, String>;
}

/// Adapter that exposes [`astra_services::CloudLlmJudge`] as [`SkillImproveLlm`].
pub(crate) struct CloudJudgeLlm(pub std::sync::Arc<astra_services::CloudLlmJudge>);

#[async_trait::async_trait]
impl SkillImproveLlm for CloudJudgeLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String, String> {
        self.0.chat_completion(system, user, 2048, 0.2).await
    }
}

/// Async variant of `check_skill_improvement_inner` with an optional LLM-driven
/// rewrite path.
///
/// Flow:
/// 1. If the auto-tuning tracker says it is not time, return.
/// 2. Collect eligible filesystem skills + recent user corrections.
/// 3. If no corrections, mark analyzed and return (quiet no-op).
/// 4. If an LLM is available and succeeds, apply the LLM-rewritten SKILL.md
///    and record a structured proposal.
/// 5. Otherwise fall back to the heuristic append ("Recent user feedback"
///    section) via [`check_skill_improvement_inner`].
async fn check_skill_improvement_async(state: &mut ReplState) {
    let llm: Option<Box<dyn SkillImproveLlm>> = state
        .matrix_runtime
        .as_ref()
        .and_then(|rt| rt.create_cloud_llm_judge())
        .map(|judge| {
            let boxed: Box<dyn SkillImproveLlm> =
                Box::new(CloudJudgeLlm(std::sync::Arc::new(judge)));
            boxed
        });

    if let Some(llm) = llm {
        match try_llm_skill_improvement(state, llm.as_ref()).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                astra_core::agent_debug!(
                    "skill",
                    "LLM skill-improvement failed, falling back to heuristic: {}",
                    e
                );
            }
        }
    }

    check_skill_improvement_inner(state);
}

/// LLM-driven skill-improvement core.
///
/// Return codes:
/// - `Ok(true)`  — the LLM path handled this turn. The caller must NOT run the
///   heuristic fallback. This covers both successful SKILL.md rewrites and
///   deliberate no-ops (no filesystem skills, no queued corrections, empty or
///   structurally-invalid LLM responses).
/// - `Err(_)`    — an unexpected error occurred; the caller should log it and
///   run the heuristic fallback.
///
/// The shape of `Result<bool, _>` is retained so future versions can reintroduce
/// an `Ok(false)` "inapplicable, please retry via heuristic" path without a
/// breaking signature change. At the moment no code path returns `Ok(false)`.
pub(crate) async fn try_llm_skill_improvement(
    state: &mut ReplState,
    llm: &dyn SkillImproveLlm,
) -> Result<bool, String> {
    if !state.skill_improvement_tracker.should_analyze(state.turn) {
        return Ok(true);
    }

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
        return Ok(true);
    }

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
        return Ok(true);
    }

    let has_correction = recent
        .iter()
        .any(|m| m.role == "user" && detect_correction_signal(&m.content));
    if !has_correction {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let target = filesystem_skills
        .iter()
        .find(|m| state.recent_tools.iter().any(|t| t.contains(&m.name)))
        .copied()
        .or_else(|| filesystem_skills.first().copied())
        .ok_or_else(|| "no target skill".to_string())?;

    let loaded = registry.get_loaded_skill(&target.name);
    let skill_dir = loaded
        .as_ref()
        .and_then(|s| s.skill_dir.clone())
        .ok_or_else(|| format!("skill {} has no on-disk directory", target.name))?;
    let skill_md = skill_dir.join("SKILL.md");
    let current_content = std::fs::read_to_string(&skill_md)
        .map_err(|e| format!("failed to read {}: {}", skill_md.display(), e))?;

    // Step 1: analysis — detect structured improvements.
    let (analysis_system, analysis_user) =
        astra_runtime::skills::improvement::build_analysis_prompt(
            &target.name,
            &current_content,
            &recent,
        );
    let analysis_resp = llm.complete(&analysis_system, &analysis_user).await?;
    let improvements = astra_runtime::skills::improvement::parse_improvements(&analysis_resp);
    if improvements.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    // Step 2: rewrite — apply improvements into a new SKILL.md.
    let rewrite_prompt =
        astra_runtime::skills::improvement::build_rewrite_prompt(&current_content, &improvements);
    let rewrite_system =
        "You are editing a skill definition file. Output only the <updated_file> block.";
    let rewrite_resp = llm.complete(rewrite_system, &rewrite_prompt).await?;
    let new_content = astra_runtime::skills::improvement::extract_updated_content(&rewrite_resp)
        .ok_or_else(|| "LLM response missing <updated_file> block".to_string())?;

    astra_runtime::skills::improvement::apply_improvement(&skill_md, &new_content)
        .map_err(|e| format!("failed to write {}: {}", skill_md.display(), e))?;

    let proposal = astra_runtime::skills::improvement::ImprovementProposal {
        skill_name: target.name.clone(),
        skill_path: skill_md.clone(),
        improvements: improvements.clone(),
    };
    state.skill_improvement_tracker.propose(proposal);
    eprintln!(
        "  {}",
        format!(
            "✓ applied {} LLM-generated improvement(s) to skill '{}' ({})",
            improvements.len(),
            target.name,
            skill_md.display()
        )
        .dim()
    );
    state.skill_improvement_tracker.mark_analyzed(state.turn);
    Ok(true)
}

/// Periodically detect user corrections in conversation history and turn them
/// into skill-improvement proposals.
///
/// After every N user turns (TURN_BATCH_SIZE), checks whether the recent conversation
/// contains corrections or improvements for any active filesystem skill.
fn check_skill_improvement(state: &mut ReplState, _line: &str, _result: &StreamResult) {
    check_skill_improvement_inner(state);
}

/// Trim the content so that at most `keep` `## Recent user feedback` sections
/// remain (the most-recent ones). A "section" is delimited by any top-level
/// `## ` heading. This prevents unbounded growth when corrections fire
/// repeatedly across long sessions.
fn trim_feedback_sections(content: &str, keep: usize) -> String {
    const HEADING: &str = "## Recent user feedback";
    if content.matches(HEADING).count() <= keep {
        return content.to_string();
    }

    // Collect byte-offsets of every `## ` heading — we only need line starts.
    let mut section_starts: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") {
            section_starts.push(pos);
        }
        pos += line.len();
    }
    section_starts.push(content.len());

    // Walk sections in order. Collect all feedback-section (start, end) pairs.
    let mut feedback_ranges: Vec<(usize, usize)> = Vec::new();
    for w in section_starts.windows(2) {
        let (s, e) = (w[0], w[1]);
        if content[s..e].trim_start().starts_with(HEADING) {
            feedback_ranges.push((s, e));
        }
    }

    if feedback_ranges.len() <= keep {
        return content.to_string();
    }

    // Drop the oldest (total - keep) ranges.
    let drop_count = feedback_ranges.len() - keep;
    let drop_set: std::collections::BTreeSet<(usize, usize)> =
        feedback_ranges.iter().take(drop_count).cloned().collect();

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for (s, e) in &drop_set {
        if cursor < *s {
            out.push_str(&content[cursor..*s]);
        }
        cursor = *e;
    }
    if cursor < content.len() {
        out.push_str(&content[cursor..]);
    }
    out
}

/// Body of `check_skill_improvement`, extracted so it can be unit-tested
/// without requiring a full `StreamResult`.
fn check_skill_improvement_inner(state: &mut ReplState) {
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

    // Heuristic closed-loop skill improvement:
    // 1. Find recent user corrections in history.
    // 2. Pick the most-recent filesystem skill (by name match in recent_tools
    //    or first-eligible if none matches).
    // 3. Append a "Recent user feedback" section to SKILL.md.
    // 4. Record the proposal on the tracker.
    //
    // This is a safe, LLM-free closed loop that ensures corrections survive
    // across sessions. The LLM-based rewrite path (build_analysis_prompt /
    // build_rewrite_prompt) is a follow-up (P1) running from a dedicated
    // async background task.

    let corrections: Vec<String> = recent
        .iter()
        .filter(|m| m.role == "user" && detect_correction_signal(&m.content))
        .map(|m| m.content.clone())
        .collect();

    if corrections.is_empty() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: {} filesystem skill(s) eligible, no user corrections in last {} messages",
            filesystem_skills.len(),
            recent.len(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    // Prefer a filesystem skill whose name appears in recent_tools; otherwise
    // fall back to the first eligible filesystem skill.
    let target = filesystem_skills
        .iter()
        .find(|m| state.recent_tools.iter().any(|t| t.contains(&m.name)))
        .copied()
        .or_else(|| filesystem_skills.first().copied());
    let Some(target) = target else {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    };

    let loaded = registry.get_loaded_skill(&target.name);
    let skill_dir = loaded.as_ref().and_then(|s| s.skill_dir.clone());
    let Some(skill_dir) = skill_dir else {
        astra_core::agent_debug!(
            "skill",
            "improvement check: skill {} has no on-disk directory — skipping",
            target.name,
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    };
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.exists() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: {} not found — skipping",
            skill_md.display(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let improvements: Vec<astra_runtime::skills::improvement::SkillImprovement> = corrections
        .iter()
        .map(|c| {
            let snippet: String = c.chars().take(240).collect();
            astra_runtime::skills::improvement::SkillImprovement {
                section: "Recent user feedback".into(),
                change: format!("User correction: {}", snippet),
                reason: "Detected correction pattern in user message".into(),
            }
        })
        .collect();

    let proposal = astra_runtime::skills::improvement::ImprovementProposal {
        skill_name: target.name.clone(),
        skill_path: skill_md.clone(),
        improvements: improvements.clone(),
    };

    // Append feedback to SKILL.md with dedup + section cap so the file doesn't
    // grow unboundedly from repeated corrections:
    //   - drop any bullet whose text already appears verbatim in the file;
    //   - keep at most MAX_FEEDBACK_SECTIONS most-recent "Recent user feedback"
    //     blocks — older ones are trimmed.
    const MAX_FEEDBACK_SECTIONS: usize = 5;
    let existing = std::fs::read_to_string(&skill_md).unwrap_or_default();

    let novel_changes: Vec<&str> = improvements
        .iter()
        .map(|imp| imp.change.as_str())
        .filter(|change| !existing.contains(change))
        .collect();

    if novel_changes.is_empty() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: all {} corrections already recorded in {} — skipping append",
            improvements.len(),
            skill_md.display(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut appended = String::new();
    appended.push_str("\n\n## Recent user feedback\n");
    appended.push_str(&format!("<!-- auto-recorded at t={} -->\n", now));
    for change in &novel_changes {
        appended.push_str(&format!("- {}\n", change));
    }

    // Trim oldest feedback sections if we'd exceed the cap after appending.
    let trimmed_existing = trim_feedback_sections(&existing, MAX_FEEDBACK_SECTIONS - 1);
    let new_content = format!("{}{}", trimmed_existing.trim_end(), appended);
    if let Err(e) = astra_runtime::skills::improvement::apply_improvement(&skill_md, &new_content) {
        eprintln!(
            "  {}",
            format!(
                "skill improvement: failed to write {}: {}",
                skill_md.display(),
                e
            )
            .yellow()
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    state.skill_improvement_tracker.propose(proposal);
    eprintln!(
        "  {}",
        format!(
            "✓ recorded {} user correction(s) into skill '{}' ({})",
            improvements.len(),
            target.name,
            skill_md.display()
        )
        .dim()
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
        Ok(events) => {
            // Write session_start if: journal is empty, last event is session_end
            // (clean restart), or we're resuming an interrupted session (no open
            // session_start). Use rposition to find the LAST start/end pair.
            let last_type = events.last().map(|e| &e.event_type);
            match last_type {
                None | Some(session_journal::JournalEventType::SessionEnd) => true,
                _ => {
                    let last_start = events.iter().rposition(|e| {
                        e.event_type == session_journal::JournalEventType::SessionStart
                    });
                    let last_end = events.iter().rposition(|e| {
                        e.event_type == session_journal::JournalEventType::SessionEnd
                    });
                    let has_unmatched_start = match (last_start, last_end) {
                        (Some(s), Some(e)) => s > e,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    !has_unmatched_start
                }
            }
        }
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
    mc.spawn_session_sync_task(async move {
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
    mc.spawn_session_sync_task(async move {
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
        assert!(effective.contains("[Active task attachment]"));
        assert!(effective.contains("debug Chinese input drops"));
        assert!(effective.contains("[User follow-up]\n继续"));
    }

    #[test]
    fn low_information_followup_detects_repair_prompts() {
        assert!(is_low_information_followup("修复?"));
        assert!(is_low_information_followup("fix this"));
        assert!(is_low_information_followup("test it"));
        assert!(!is_low_information_followup("修一下输入法问题"));
        assert!(!is_low_information_followup(
            "implement request batching in runtime selector"
        ));
    }

    #[test]
    fn build_effective_line_injects_attachment_for_low_information_repair_followup() {
        let state = ReplState {
            continuation_anchor: Some(
                "Latest user task: review commit aa1f419b\nLatest assistant summary:\n## Review\nP5 still blocks large merges"
                    .to_string(),
            ),
            ..ReplState::default()
        };

        let effective = build_effective_line("修复?", &state);
        assert!(effective.contains("[Active task attachment]"));
        assert!(effective.contains("review commit aa1f419b"));
        assert!(effective.contains("fix / patch / test / continue"));
        assert!(effective.contains("[User follow-up]\n修复?"));
    }

    #[test]
    fn build_effective_line_leaves_normal_prompt_untouched() {
        let state = ReplState {
            continuation_anchor: Some("Latest user task: debug Chinese input drops".to_string()),
            ..ReplState::default()
        };

        let effective = build_effective_line("修一下输入法问题", &state);
        assert!(!effective.contains("[Active task attachment]"));
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
            effective.contains("[Active task attachment]"),
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
        assert!(effective_no_goal.contains("[Active task attachment]"));
        assert!(!effective_no_goal.contains("Session goal:"));
    }

    #[test]
    fn consume_plan_resume_if_matches_requires_resume_signal() {
        let mut state = ReplState {
            pending_plan_resume_digest: Some("[plan-resume] goal=\"Fix auth\"".to_string()),
            ..ReplState::default()
        };

        assert!(consume_plan_resume_if_matches(&mut state, "tell me a joke").is_none());
        assert_eq!(
            state.pending_plan_resume_digest.as_deref(),
            Some("[plan-resume] goal=\"Fix auth\"")
        );
    }

    #[test]
    fn consume_plan_resume_if_matches_consumes_digest_on_explicit_tag() {
        let mut state = ReplState {
            pending_plan_resume_digest: Some("[plan-resume] goal=\"Fix auth\"".to_string()),
            ..ReplState::default()
        };

        let digest = consume_plan_resume_if_matches(&mut state, "please @resume-plan");
        assert_eq!(digest.as_deref(), Some("[plan-resume] goal=\"Fix auth\""));
        assert!(state.pending_plan_resume_digest.is_none());
    }

    #[test]
    fn apply_resume_context_injects_implicit_resume_tag_and_digest() {
        let effective = apply_resume_context(
            "continue".to_string(),
            None,
            Some("[plan-resume] goal=\"Fix auth\"".to_string()),
        );
        assert!(effective.starts_with("@resume-plan\n[plan-resume]"));
        assert!(effective.ends_with("\n\ncontinue"));
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
        assert!(effective.contains("[Active task attachment]"), "anchor");
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
            effective.contains("[Active task attachment]"),
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
            !normal.contains("[Active task attachment]"),
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
            turn_observability_events: Vec::new(),
            llm_rounds: None,
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
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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

    /// Verifies build_continuation_anchor keeps a bounded, multi-line attachment.
    #[test]
    fn continuation_anchor_builder_truncates_long_content() {
        let long_user_input = "a".repeat(300);
        let long_assistant = format!(
            "{}\nSecond line of detail\nThird line of detail\nFourth line should be dropped",
            "b".repeat(300)
        );

        let state = ReplState::default();
        let mut result = stub_stream_result(&long_assistant);
        result.tools_used = vec!["read_file".into(), "str_replace".into()];
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("rust/crates/runtime/src/server/run_lifecycle.rs".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let anchor = build_continuation_anchor(&state, &long_user_input, &result);
        let anchor = anchor.expect("should produce anchor");

        assert!(anchor.contains("Latest user task: "));
        let user_part = anchor
            .split("Latest user task: ")
            .nth(1)
            .unwrap()
            .split('\n')
            .next()
            .unwrap();
        assert_eq!(user_part.chars().count(), 221);

        assert!(anchor.contains("Latest assistant summary:\n"));
        assert!(anchor.contains("Second line of detail"));
        assert!(anchor.contains("Third line of detail"));
        assert!(!anchor.contains("Fourth line should be dropped"));
        assert!(anchor.contains("Recent tools: read_file, str_replace"));
        assert!(
            anchor
                .contains("Artifact: read_file → rust/crates/runtime/src/server/run_lifecycle.rs")
        );
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
            "Latest user task: explain ownership\nLatest assistant summary:\nOwnership in Rust means each value has exactl"
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
            "Latest user task: now explain borrowing\nLatest assistant summary:\nBorrowing lets you reference data"
                .to_string(),
        );
        let effective = build_effective_line("continue", &state);
        assert!(effective.contains("[Active task attachment]"));
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

    // ─── E2E: skill improvement closed loop ─────────────────────────────
    // Seeds a tempdir filesystem skill, injects a user correction into
    // ReplState.history, and calls `check_skill_improvement_inner` to verify:
    //   1. SKILL.md on disk now contains a "Recent user feedback" section.
    //   2. ImprovementTracker.pending_proposal is populated.
    //   3. Tracker advanced past `should_analyze`.
    #[tokio::test]
    async fn skill_improvement_records_correction_on_filesystem_skill() {
        // 1. Build tempdir skills root: <tmp>/my-skill/SKILL.md
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: my-skill\ndescription: test skill\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\n",
        )
        .unwrap();

        // 2. Build a registry backed by a LocalSkillProvider pointing at tmp.
        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        // Pre-load so skill_dir is populated on the cached LoadedSkill.
        registry.load("my-skill").await.unwrap();
        assert_eq!(registry.len(), 1);

        // 3. Wire up a ReplState with the registry, a correction message
        //    in history, and a turn count past the batch threshold.
        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, that's wrong — please do it differently next time".to_string(),
                "(previous assistant response)".to_string(),
            )],
            turn: astra_runtime::skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };
        assert!(state.skill_improvement_tracker.should_analyze(state.turn));

        // 4. Run the closed loop.
        check_skill_improvement_inner(&mut state);

        // 5a. SKILL.md should now contain the auto-recorded feedback.
        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            updated.contains("Recent user feedback"),
            "SKILL.md should contain feedback section:\n{}",
            updated
        );
        assert!(
            updated.contains("User correction:"),
            "SKILL.md should quote the user correction:\n{}",
            updated
        );
        assert!(
            updated.contains("Original instructions."),
            "SKILL.md must preserve original content:\n{}",
            updated
        );

        // 5b. The tracker should hold the pending proposal for UI surfacing.
        let pending = state
            .skill_improvement_tracker
            .take_proposal()
            .expect("pending proposal should be recorded");
        assert_eq!(pending.skill_name, "my-skill");
        assert_eq!(pending.skill_path, skill_md);
        assert!(!pending.improvements.is_empty());

        // 5c. Tracker advanced past should_analyze.
        assert!(
            !state.skill_improvement_tracker.should_analyze(state.turn),
            "tracker must advance last_analyzed_count"
        );
    }

    // Regression guard: when there are no corrections, the loop must NOT
    // touch SKILL.md and must NOT record a proposal.
    #[tokio::test]
    async fn skill_improvement_noop_without_correction() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test skill\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "hello, please add a feature".to_string(),
                "sure thing".to_string(),
            )],
            turn: astra_runtime::skills::improvement::TURN_BATCH_SIZE + 1,
            ..Default::default()
        };

        check_skill_improvement_inner(&mut state);

        let unchanged = std::fs::read_to_string(&skill_md).unwrap();
        assert_eq!(unchanged, original, "SKILL.md must not be modified");
        assert!(state.skill_improvement_tracker.pending_proposal.is_none());
    }

    // ─── E2E: user correction → EvolutionSignal::UserCorrection emission ───
    #[tokio::test]
    async fn emit_user_correction_pushes_signal_to_evolution_service() {
        let evo = std::sync::Arc::new(astra_runtime::evolution::service::EvolutionService::new());
        let state = ReplState {
            evolution_service: Some(evo.clone()),
            history: vec![(
                "write a function".to_string(),
                "here is the function".to_string(),
            )],
            recent_tools: vec!["filesystem".to_string()],
            turn: 3,
            ..Default::default()
        };

        emit_user_correction_signal(&state, "no, that's wrong, do it differently").await;

        let (_fast, llm_routed) = evo.flush().await;
        // UserCorrection is LLM-routed by needs_llm (contains skill_context).
        let found = llm_routed.iter().any(|s| {
            matches!(
                s,
                astra_runtime::evolution::types::EvolutionSignal::UserCorrection {
                    skill_context: Some(sc),
                    correction_text,
                    prior_assistant_text,
                    turn_id,
                } if sc == "filesystem"
                    && correction_text.starts_with("no, that's wrong")
                    && prior_assistant_text == "here is the function"
                    && turn_id == "turn-3"
            )
        });
        assert!(
            found,
            "UserCorrection signal not found in flushed llm_routed: {:?}",
            llm_routed
        );
    }

    #[tokio::test]
    async fn emit_user_correction_noop_without_evolution_service() {
        let state = ReplState {
            evolution_service: None,
            history: vec![("u".to_string(), "a".to_string())],
            ..Default::default()
        };
        // Must not panic.
        emit_user_correction_signal(&state, "no, that's wrong").await;
    }

    // ─── E2E: LLM-driven skill improvement ───────────────────────────────
    //
    // Seeds a tempdir skill, injects a correction + a fake LLM that returns
    // structured improvements and a rewritten SKILL.md, then exercises the
    // production closed loop via `try_llm_skill_improvement`.
    //
    // Verifies:
    //   * LLM-rewritten content replaces the original SKILL.md body.
    //   * Structured `ImprovementProposal` (from parsed JSON) lands in tracker.
    //   * Tracker advances past `should_analyze`.

    struct FakeLlm {
        responses: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SkillImproveLlm for FakeLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String, String> {
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() {
                Err("no canned response".into())
            } else {
                Ok(r.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn llm_skill_improvement_rewrites_skill_md_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, don't greet twice — skip the greeting on follow-ups".to_string(),
                "Hello again!".to_string(),
            )],
            turn: astra_runtime::skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        // Canned LLM responses: first = analysis JSON, second = rewritten file.
        let analysis = r#"[
          {"section": "greeting", "change": "skip greeting on follow-ups", "reason": "user said don't greet twice"}
        ]"#;
        let rewritten = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\nSkip greeting on follow-up turns per user preference.\n";
        let wrapped_rewrite = format!("<updated_file>\n{}\n</updated_file>", rewritten);

        let llm = FakeLlm {
            responses: std::sync::Mutex::new(vec![analysis.to_string(), wrapped_rewrite]),
        };

        let ok = try_llm_skill_improvement(&mut state, &llm)
            .await
            .expect("LLM path should succeed");
        assert!(ok, "LLM path should report handled=true");

        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            updated.contains("Skip greeting on follow-up turns"),
            "SKILL.md should contain LLM-rewritten body:\n{}",
            updated
        );
        assert!(
            updated.contains("name: my-skill"),
            "frontmatter must be preserved:\n{}",
            updated
        );

        let pending = state
            .skill_improvement_tracker
            .take_proposal()
            .expect("structured proposal should land in tracker");
        assert_eq!(pending.skill_name, "my-skill");
        assert_eq!(pending.improvements.len(), 1);
        assert_eq!(pending.improvements[0].section, "greeting");

        assert!(
            !state.skill_improvement_tracker.should_analyze(state.turn),
            "tracker must advance"
        );
    }

    // When LLM returns empty improvements array, the loop should no-op
    // (not rewrite SKILL.md) but still mark analyzed.
    #[tokio::test]
    async fn llm_skill_improvement_empty_response_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![("no, that's wrong".to_string(), "sorry".to_string())],
            turn: astra_runtime::skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        let llm = FakeLlm {
            responses: std::sync::Mutex::new(vec!["[]".to_string()]),
        };

        let ok = try_llm_skill_improvement(&mut state, &llm).await.unwrap();
        assert!(ok);

        assert_eq!(
            std::fs::read_to_string(&skill_md).unwrap(),
            original,
            "SKILL.md must be unchanged on empty improvements"
        );
        assert!(state.skill_improvement_tracker.take_proposal().is_none());
    }

    /// Verify that when the LLM path errors out, the caller falls back to
    /// the heuristic: SKILL.md receives the "Recent user feedback" section
    /// and the tracker gets a heuristic-shaped proposal.
    #[tokio::test]
    async fn llm_error_falls_back_to_heuristic() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, that's wrong, do it differently".to_string(),
                "sorry".to_string(),
            )],
            turn: astra_runtime::skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        let llm = FakeLlm {
            // Empty canned responses → the fake returns Err on first call,
            // mimicking a cloud LLM outage.
            responses: std::sync::Mutex::new(vec![]),
        };

        let result = try_llm_skill_improvement(&mut state, &llm).await;
        assert!(
            result.is_err(),
            "expected LLM path to return Err, got: {:?}",
            result
        );

        // Simulate the caller's fallback to the heuristic.
        check_skill_improvement_inner(&mut state);

        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(
            updated.contains("## Recent user feedback"),
            "heuristic fallback should have appended feedback section; got: {}",
            updated
        );
        assert!(
            updated.contains("no, that's wrong"),
            "feedback section should contain correction snippet; got: {}",
            updated
        );
    }

    #[test]
    fn trim_feedback_sections_caps_at_keep() {
        let content = "# Body\ncontent\n\n## Recent user feedback\n- a\n\n\
                       ## Recent user feedback\n- b\n\n## Recent user feedback\n- c\n\n\
                       ## Recent user feedback\n- d\n";
        // keep=2 → only the two most-recent sections (c, d) should remain.
        let trimmed = trim_feedback_sections(content, 2);
        assert_eq!(trimmed.matches("## Recent user feedback").count(), 2);
        assert!(!trimmed.contains("- a"));
        assert!(!trimmed.contains("- b"));
        assert!(trimmed.contains("- c"));
        assert!(trimmed.contains("- d"));
        // Non-feedback content preserved.
        assert!(trimmed.contains("# Body"));
    }

    #[test]
    fn trim_feedback_sections_noop_when_within_cap() {
        let content = "# Body\n\n## Recent user feedback\n- only\n";
        let trimmed = trim_feedback_sections(content, 5);
        assert_eq!(trimmed, content);
    }

    #[test]
    fn stall_type_confidence_maps_known_signals() {
        // Deterministic signals → full confidence.
        assert_eq!(super::stall_type_confidence("sig_stall"), 1.0);
        assert_eq!(super::stall_type_confidence("skill_lockout"), 1.0);
        assert_eq!(
            super::stall_type_confidence("skill_lockout:review-changes"),
            1.0
        );
        assert_eq!(
            super::stall_type_confidence("skill_lockout:any-other-skill"),
            1.0
        );
        assert_eq!(super::stall_type_confidence("skill_lockout:"), 1.0);

        // Heuristic / unknown types must stay at 0.0 so journal write-through
        // skips emission (avoids polluting downstream reflection pipelines).
        assert_eq!(super::stall_type_confidence("repetition_stall"), 0.0);
        assert_eq!(super::stall_type_confidence("name_stall"), 0.0);
        assert_eq!(super::stall_type_confidence(""), 0.0);
        assert_eq!(super::stall_type_confidence("unknown_type"), 0.0);
        // Near-miss must not match — prefix is literal.
        assert_eq!(super::stall_type_confidence("skill_lockou"), 0.0);
        // Underscore suffix must not match — only exact or colon suffix.
        assert_eq!(super::stall_type_confidence("skill_lockout_v2"), 0.0);
    }
}
