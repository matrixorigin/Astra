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

/// First-turn gate for cross-session lesson loading (P6). Returns true when
/// the cache is empty AND the session has a populated lesson source. Kept
/// pure so a unit test can pin the trigger condition.
/// Run the lesson checkpointer against the current session signals.
/// If new lessons are produced, fire-and-forget write them to both
/// agent_lessons (local) and Memoria (L3). Never blocks the turn.
fn maybe_checkpoint_lessons(state: &mut ReplState) {
    let summary = match state
        .observability_session
        .as_ref()
        .and_then(|arc| arc.read().ok())
    {
        Some(guard) => astra_runtime::lesson_extractor::summarise_from_runtime(
            &state.tool_health_entries,
            Some(&*guard),
        ),
        None => astra_runtime::lesson_extractor::summarise_from_runtime(
            &state.tool_health_entries,
            None,
        ),
    };

    let delta = state.lesson_checkpointer.maybe_checkpoint(
        &summary,
        state.turn,
        state.ingestion_user_id.as_deref().unwrap_or("unknown"),
        "generic",
        None,
    );

    if delta.is_empty() {
        return;
    }

    // Write to Memoria as `working` memory (session-scoped, T4).
    // Basic quality gate (hedging + length). Template blocklist NOT applied
    // here — these are deterministic template lessons, not LLM output.
    // Promoted to semantic T3 at session end via final checkpoint flush.
    let memoria_lessons: Vec<astra_runtime::lesson_synthesizer::ExtractedLesson> = delta
        .into_iter()
        .filter(|l| astra_runtime::lesson_synthesizer::is_high_quality_lesson(&l.action))
        .map(|l| astra_runtime::lesson_synthesizer::ExtractedLesson {
            memory_type: "working",
            content: format!("💡 LESSON: {}", l.action),
            trust_tier: "T4",
        })
        .collect();
    if memoria_lessons.is_empty() {
        return;
    }
    let sid = state.session_id.clone();
    tokio::spawn(
        super::edge_tools::memoria::memoria_store_lessons_fire_and_forget(memoria_lessons, sid),
    );
}

/// Filter lessons through a cheap selector model for relevance.
/// Requires ASTRA_SELECTOR_MODEL_URL + ASTRA_SELECTOR_MODEL_KEY + ASTRA_SELECTOR_MODEL env vars.
/// Falls back to returning all lessons on any error, timeout, or missing config.
async fn filter_lessons_by_relevance(
    user_message: &str,
    lessons: Vec<astra_runtime::self_model::LessonHint>,
    _matrix_runtime: Option<&std::sync::Arc<astra_runtime::MatrixCloudRuntime>>,
) -> Vec<astra_runtime::self_model::LessonHint> {
    let base_url = match std::env::var("ASTRA_SELECTOR_MODEL_URL").ok() {
        Some(u) if !u.is_empty() => u,
        _ => return lessons,
    };
    let api_key = std::env::var("ASTRA_SELECTOR_MODEL_KEY").unwrap_or_default();
    let model_name = std::env::var("ASTRA_SELECTOR_MODEL").unwrap_or_else(|_| "qwen-flash".into());

    let memory_texts: Vec<String> = lessons.iter().map(|l| l.action.clone()).collect();
    let query = astra_runtime::memory_relevance::build_relevance_query(user_message, &memory_texts);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return lessons,
    };

    let resp = match client
        .post(format!("{base_url}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [
                {"role": "system", "content": astra_runtime::memory_relevance::RELEVANCE_FILTER_PROMPT},
                {"role": "user", "content": query},
            ],
            "max_tokens": 50,
            "temperature": 0.0,
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return lessons,
    };

    let body = resp.text().await.unwrap_or_default();
    let text = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .get(0)?
                .get("message")?
                .get("content")?
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default();

    if text.is_empty() {
        return lessons;
    }

    let indices = astra_runtime::memory_relevance::parse_relevance_response(&text, lessons.len());
    if indices.is_empty() {
        return lessons;
    }

    astra_runtime::memory_relevance::filter_by_indices(&lessons, &indices)
}

fn should_bootstrap_lessons(state: &ReplState) -> bool {
    !state.session_lessons_loaded
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

fn is_greeting_like_message(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "hi" | "hello"
            | "hey"
            | "hey!"
            | "hello!"
            | "hi!"
            | "good morning"
            | "good afternoon"
            | "good evening"
    ) || matches!(trimmed, "你好" | "您好" | "嗨" | "哈喽")
}

fn session_goal_candidate(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || is_greeting_like_message(trimmed)
        || is_low_information_followup(trimmed)
    {
        return None;
    }

    Some(truncate_chars(trimmed, 220))
}

fn session_goal_is_placeholder(goal: &str) -> bool {
    let trimmed = goal.trim();
    trimmed.is_empty() || is_greeting_like_message(trimmed)
}

fn maybe_update_session_goal(state: &mut ReplState, line: &str) {
    let Some(candidate) = session_goal_candidate(line) else {
        return;
    };

    match state.session_goal.as_deref() {
        None => state.session_goal = Some(candidate),
        Some(existing) if session_goal_is_placeholder(existing) => {
            state.session_goal = Some(candidate);
        }
        Some(_) => {}
    }
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

    // ─── Bootstrap cross-session lessons from Memoria on first turn ────────
    // Memoria is the single source of truth for lessons (Session Memory
    // Protocol L3). agent_lessons table is no longer used for bootstrap.
    if should_bootstrap_lessons(state) {
        let lessons = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            super::edge_tools::memoria::memoria_retrieve_lessons(6, Some(message)),
        )
        .await
        .unwrap_or_default();
        // Relevance filter: if we have lessons AND a selector model, filter noise.
        // Best-effort: timeout or model unavailable → keep all lessons.
        state.session_lessons = if lessons.len() > 1 {
            filter_lessons_by_relevance(message, lessons, state.matrix_runtime.as_ref()).await
        } else {
            lessons
        };
        state.session_lessons_loaded = true;
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

        // Incremental lesson checkpoint: user correction is a high-value
        // breakpoint — extract any new lessons NOW rather than waiting for
        // session end. Fire-and-forget: never blocks the user's turn.
        maybe_checkpoint_lessons(state);
    }

    // Create a cancellation token that can interrupt SSE streaming mid-flight.
    let cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    let cancel_token_for_signal = cancel_token.clone();

    // Clone observability context for the turn
    let obs_hub = state.observability_hub.clone();
    let obs_session = state.observability_session.clone();

    let attempt = tokio::select! {
        result = stream_chat_sse(ChatTurnParams {
            api: ctx.api,
            token,
            auth_profile: ctx.profile,
            message,
            session_id,
            model: state.model.as_deref(),
            provider: None,
            explain: state.explain,
            render_md: true,
            history: &state.history,
            perm_manager: &mut state.perm_manager,
            verbose_mode: state.verbose_mode,
            render_policy: crate::stream_render::RenderPolicy::Stream,
            selector: ctx.selector,
            recent_tools: &state.recent_tools,
            tool_health_entries: &state.tool_health_entries,
            session_lessons: &state.session_lessons,
            latest_skill_diagnosis: state.latest_skill_diagnosis.as_ref(),
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
            runtime_continuity: state.runtime_continuity.as_ref(),
            turn_index: state.turn,
            evolution_service: state.evolution_service.clone(),
            pre_loaded_messages: None,
        }) => TurnAttempt::Completed(Box::new(result)),
        _ = tokio::signal::ctrl_c() => {
            // Trigger cancellation to interrupt any in-flight SSE streaming.
            cancel_token_for_signal.cancel();
            eprintln!("\n{}", "  Interrupted.".dim());
            TurnAttempt::Interrupted
        }
    };

    // ─── P8: auto-invoke diagnostic skills at turn end ──────────────────────
    // Compute SessionSignals from live observability, fire any triggered
    // skills through the handler, stash the first diagnosis for the next
    // turn's ToolExecutor seam. Best-effort: any failure at any step is
    // logged and swallowed so an auto-invoke problem can never break the
    // turn completion path.
    // Skip auto-invoke on Ctrl+C — the user expects immediate return,
    // and mutex acquisition + signal computation would add perceptible
    // latency to the interrupt response path.
    if !matches!(attempt, TurnAttempt::Interrupted) {
        maybe_run_auto_invoke(state).await;
    }

    attempt
}

/// Run the auto-invoke handler once per turn: compute signals, fire the
/// gate, stash the first diagnosis in `state.latest_skill_diagnosis` for
/// the next turn's prompt. Lazily creates the handler on first use so
/// sessions that never trigger anything pay no cost.
async fn maybe_run_auto_invoke(state: &mut ReplState) {
    // Snapshot signals under a scoped read lock so we don't hold the
    // observability lock across the await on maybe_fire.
    let signals = match state.observability_session.as_ref() {
        Some(arc) => match arc.read() {
            Ok(guard) => astra_runtime::auto_invoke_handler::compute_session_signals(Some(&*guard)),
            Err(_poison) => {
                tracing::warn!(
                    target: "auto_invoke",
                    "observability_session lock poisoned; auto-invoke signals degraded",
                );
                astra_runtime::auto_invoke_handler::compute_session_signals(None)
            }
        },
        None => astra_runtime::auto_invoke_handler::compute_session_signals(None),
    };

    // R1: evaluate active diagnosis postconditions BEFORE gating on
    // default signals, because the tracker may have pending criteria
    // even when no new signals fired this turn.
    let outcomes = state
        .diagnosis_outcome_tracker
        .evaluate_turn(&signals, state.turn);
    for outcome in &outcomes {
        let met = outcome
            .statuses
            .iter()
            .filter(|s| {
                **s == astra_runtime::auto_invoke_handler::DiagnosisCriterionStatus::Satisfied
            })
            .count() as u32;
        let failed = outcome
            .statuses
            .iter()
            .filter(|s| **s == astra_runtime::auto_invoke_handler::DiagnosisCriterionStatus::Failed)
            .count() as u32;
        state.diagnosis_criteria_met = state.diagnosis_criteria_met.saturating_add(met);
        state.diagnosis_criteria_failed = state.diagnosis_criteria_failed.saturating_add(failed);
    }

    // Fast-path: no signals worth even locking the handler for.
    // Also clear any stale diagnosis so it doesn't linger in the prompt
    // for the rest of a healthy session.
    let default = astra_skills::auto_invoke::SessionSignals::default();
    if signals == default {
        state.latest_skill_diagnosis = None;
        return;
    }

    // Lazy-create the handler on first use.
    if state.auto_invoke_handler.is_none() {
        let exec: std::sync::Arc<dyn astra_runtime::auto_invoke_handler::SkillExecutor> =
            std::sync::Arc::new(
                astra_runtime::auto_invoke_handler::SyntheticSkillDiagnosisExecutor,
            );
        state.auto_invoke_handler = Some(
            astra_runtime::auto_invoke_handler::AutoInvokeHandler::new(exec),
        );
    }

    let handler = state
        .auto_invoke_handler
        .as_mut()
        .expect("just constructed above");
    let diagnoses = handler
        .maybe_fire(&signals, std::time::Instant::now())
        .await;

    // Activate ALL returned diagnoses in the tracker so their
    // success_criteria are evaluated on subsequent turns, even if we
    // only render the first in the prompt.
    for diag in &diagnoses {
        state
            .diagnosis_outcome_tracker
            .activate(diag.clone(), signals, state.turn);
    }
    // Render the first diagnosis in the prompt. If the handler returned
    // nothing (cooldown), keep the previous diagnosis visible so the LLM
    // sees what it's being measured against while the tracker evaluates.
    if let Some(diag) = diagnoses.into_iter().next() {
        state.latest_skill_diagnosis = Some(diag);
    }
    // Don't clear latest_skill_diagnosis on cooldown — the tracker is
    // still evaluating. It gets cleared in the zero-signals fast path
    // above, or when a new diagnosis replaces it.
}

/// Build a compact tool-call summary for cross-turn context continuity.
///
/// Appended to the assistant text in history so the next turn's prompt
/// contains file paths and tool outcomes from the previous turn — without
/// storing the full tool_call / tool_result messages.
fn build_turn_tool_summary(records: &[session_journal::ToolCallRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }

    // Collect unique file paths (preserving first-seen order).
    let mut files: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for r in records {
        if let Some(fp) = r.file_path.as_deref() {
            if !files.contains(&fp) {
                files.push(fp);
            }
        }
        if !r.ok && !failed.contains(&r.name.as_str()) {
            failed.push(&r.name);
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !files.is_empty() {
        if files.len() <= 15 {
            parts.push(format!("files: {}", files.join(", ")));
        } else {
            let shown: Vec<&str> = files[..15].to_vec();
            parts.push(format!(
                "files: {} (+{} more)",
                shown.join(", "),
                files.len() - 15
            ));
        }
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", failed.join(", ")));
    }
    let tool_count = records.len();
    parts.push(format!("tool_calls: {tool_count}"));

    format!("\n\n[Turn context: {}]", parts.join(" | "))
}

/// Build the text stored in history: assistant response + optional tool summary.
fn build_history_text(full_text: &str, records: &[session_journal::ToolCallRecord]) -> String {
    let summary = build_turn_tool_summary(records);
    if summary.is_empty() {
        return full_text.to_string();
    }
    format!("{full_text}{summary}")
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
    // Capture stall flag before entering the journal borrow scope.
    let has_stalls = !result.stall_events.is_empty();

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
        if let Some(interruption) = result.interruption.as_ref() {
            turn_event.metadata = Some(merge_interruption_metadata(
                turn_event.metadata.take(),
                interruption,
            ));
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
                    let svc = std::sync::Arc::clone(mc.sync_service());
                    let sid_owned = sid.to_string();
                    let user_id_owned = user_id.to_string();
                    let cp_clone = cp.clone();
                    let cp_number = cp.number;
                    mc.spawn_session_sync_task(async move {
                        if let Err(e) = svc.push_checkpoint(
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
                let svc = std::sync::Arc::clone(mc.sync_service());
                let sid_owned = sid.to_string();
                let user_id_owned = user_id.to_string();
                let cp_number = result
                    .step_recorder_summary
                    .as_ref()
                    .map(|s| s.checkpoints)
                    .unwrap_or(0);
                let (tier, turn, title, tools_json): (String, u32, String, String) = match step_cp {
                    astra_pipeline::step_protocol::StepCheckpoint::Light(l) => (
                        "light".to_string(),
                        0u32,
                        format!("step:{}", l.step_id),
                        "[]".to_string(),
                    ),
                    astra_pipeline::step_protocol::StepCheckpoint::Heavy(h) => {
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
                    if let Err(e) = svc.push_step_checkpoint(
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
                let svc = std::sync::Arc::clone(mc.sync_service());
                let sid_owned = sid.to_string();
                let user_id_owned = user_id.to_string();
                let trace_signal = trace_signal.clone();
                mc.spawn_session_sync_task(async move {
                    if let Err(e) = svc.push_context_trace_signal(
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
                let svc = std::sync::Arc::clone(mc.sync_service());
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
                    if let Err(e) = svc
                        .push_session_state(
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

    // Incremental lesson checkpoint: stall events signal that the agent
    // struggled — extract any new lessons NOW. Runs outside the journal
    // borrow scope so it can take &mut state.
    if has_stalls {
        maybe_checkpoint_lessons(state);
    }
}

fn merge_interruption_metadata(
    existing: Option<serde_json::Value>,
    interruption: &serde_json::Value,
) -> serde_json::Value {
    let mut metadata = match existing {
        Some(serde_json::Value::Object(map)) => map,
        Some(value) => {
            let mut map = serde_json::Map::new();
            map.insert("previous_metadata".into(), value);
            map
        }
        None => serde_json::Map::new(),
    };
    metadata.insert("partial".into(), serde_json::json!(true));
    metadata.insert("interrupted".into(), serde_json::json!(true));
    if let Some(kind) = interruption.get("kind").and_then(|value| value.as_str()) {
        metadata.insert("interruption_kind".into(), serde_json::json!(kind));
    }
    metadata.insert("interruption".into(), interruption.clone());
    serde_json::Value::Object(metadata)
}

/// Routing + turn quality for journal fields and `ToolSelector::record_outcome`.
pub(super) struct ReplTurnLearningSnapshot {
    pub routing: astra_turn_core::routing_engine::RoutingDecision,
    pub eval: astra_runtime::pipeline::evaluation::TurnEvaluation,
}

pub(super) fn analyze_repl_turn_learning(
    line: &str,
    turn: u32,
    recent_tools: &[String],
    result: &StreamResult,
) -> ReplTurnLearningSnapshot {
    use astra_runtime::pipeline::evaluation::{
        TurnEvaluationTelemetry, current_evaluation_thresholds,
        evaluate_tool_call_records_with_thresholds_and_telemetry,
    };
    use astra_turn_core::routing_engine::RoutingEngine;
    let routing = RoutingEngine::analyze(line, turn, recent_tools, &[], vec![]);

    let has_verdict_warning = result.verdict_events.iter().any(|v| {
        v.severity.eq_ignore_ascii_case("warning") || v.severity.eq_ignore_ascii_case("critical")
    });

    let mut first_round_prompt_tokens: Option<u64> = None;
    let mut max_round_prompt_tokens: Option<u64> = None;
    for event in &result.turn_observability_events {
        if event.event_type != session_journal::JournalEventType::LlmRound {
            continue;
        }
        let source = event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("source"))
            .and_then(serde_json::Value::as_str);
        if source != Some("agentic_loop") {
            continue;
        }
        let Some(tokens_in) = event.tokens_in else {
            continue;
        };
        first_round_prompt_tokens.get_or_insert(tokens_in);
        max_round_prompt_tokens = Some(
            max_round_prompt_tokens
                .map(|current| current.max(tokens_in))
                .unwrap_or(tokens_in),
        );
    }

    let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
        line,
        recent_tools,
        &result.tool_call_records,
        result.stall_events.len(),
        has_verdict_warning,
        result.budget_pressure,
        current_evaluation_thresholds(),
        TurnEvaluationTelemetry {
            llm_rounds: result.llm_rounds,
            prompt_tokens: Some(result.prompt_tokens),
            first_round_prompt_tokens,
            max_round_prompt_tokens,
        },
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
    let signal = astra_turn_types::detect_implicit_feedback_signal(line, prev_assistant_text);
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
    mut result: StreamResult,
    turn_start: Instant,
) {
    let final_messages = std::mem::take(&mut result.final_messages);
    let csl_checkpoint_fields = extract_csl_fields_from_result(&result);
    apply_turn_success_sync(state, selector, profile, line, result, turn_start);

    // Persist CSL via CslManager.
    let turn = state.turn;
    let prev_state = state
        .csl_manager
        .as_ref()
        .map(|m| m.last_session_state().clone())
        .unwrap_or_default();
    let session_state = build_full_session_state_compact(state, csl_checkpoint_fields, &prev_state);
    if let Some(mgr) = state.csl_manager.as_mut() {
        if let Err(e) = mgr
            .persist_turn(turn, &final_messages, &session_state)
            .await
        {
            astra_core::agent_warn!("csl", "persist failed: {e}");
        }
    }
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

        if state.csl_manager.is_none() {
            let store = std::sync::Arc::new(
                astra_turn_core::conversation_log::file_store::FileCslStore::new(
                    astra_services::session_journal::local_sessions_dir(),
                ),
            );
            state.csl_manager = match astra_turn_core::conversation_log::manager::CslManager::new(
                store,
                session_id.to_string(),
                Default::default(),
            ) {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    astra_core::agent_warn!("csl", "manager init failed: {e}");
                    None
                }
            };
        }

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
    state.runtime_continuity = Some(result.runtime_continuity.clone());
    state.last_response = Some(result.full_text.clone());
    state.continuation_anchor = build_continuation_anchor(state, line, &result);
    state.pending_followup_suggestion =
        crate::followup_suggestion::suggest_followup(line, state, &result);
    if let Some(suggestion) = state.pending_followup_suggestion.as_ref() {
        super::repl_ui::set_followup_prompt_hint(Some(suggestion.text.clone()));
    } else {
        super::repl_ui::clear_followup_prompt_hint();
    }

    maybe_update_session_goal(state, line);
    // New user input invalidates redo stack (history diverged)
    state.redo_stack.clear();
    state.history.push((
        line.to_string(),
        build_history_text(&result.full_text, &result.tool_call_records),
    ));
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
        .map(|d| astra_turn_core::routing_engine::domain_hint_to_label(d).to_string());
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

    // Display billable totals: `↑` is the WHOLE input (fresh + cached + creation)
    // because all three occupy the context window and are all billed (just at
    // different rates). Showing only `prompt_tokens` (fresh) hides the dominant
    // portion of real traffic on cache-heavy turns and makes cache% nonsensical.
    let total_input =
        result.prompt_tokens + result.cache_read_tokens + result.cache_creation_tokens;
    let total_tokens = total_input + result.completion_tokens;
    let tokens_str = if total_tokens > 1000 {
        format!("{:.1}k", total_tokens as f64 / 1000.0)
    } else {
        format!("{total_tokens}")
    };
    let prompt_short = if total_input > 1000 {
        format!("{:.1}k", total_input as f64 / 1000.0)
    } else {
        format!("{total_input}")
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

    // Cache hit rate across the full billable input. Denominator must include
    // ALL three input buckets (fresh + cache read + cache creation), otherwise
    // cache_creation-heavy turns report 100% when they actually wrote a lot.
    if result.cache_read_tokens > 0 {
        let cache_pct = result.cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0;
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
    // LLM-driven skill improvement previously used CloudLlmJudge::from_env; after env cleanup
    // it falls through to the heuristic append path below. TODO: wire a server-proxy LLM client
    // here if skill auto-rewrite becomes a priority again.
    let llm: Option<Box<dyn SkillImproveLlm>> = None;

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
        .filter(|m| matches!(m.source, astra_skills::manifest::SkillSourceKind::Local))
        .collect();
    if filesystem_skills.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let recent: Vec<astra_skills::improvement::RecentMessage> = state
        .history
        .iter()
        .rev()
        .take(astra_skills::improvement::TURN_BATCH_SIZE as usize)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                astra_skills::improvement::RecentMessage {
                    role: "user".into(),
                    content: user.clone(),
                },
                astra_skills::improvement::RecentMessage {
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
        astra_skills::improvement::build_analysis_prompt(&target.name, &current_content, &recent);
    let analysis_resp = llm.complete(&analysis_system, &analysis_user).await?;
    let improvements = astra_skills::improvement::parse_improvements(&analysis_resp);
    if improvements.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    // Step 2: rewrite — apply improvements into a new SKILL.md.
    let rewrite_prompt =
        astra_skills::improvement::build_rewrite_prompt(&current_content, &improvements);
    let rewrite_system =
        "You are editing a skill definition file. Output only the <updated_file> block.";
    let rewrite_resp = llm.complete(rewrite_system, &rewrite_prompt).await?;
    let new_content = astra_skills::improvement::extract_updated_content(&rewrite_resp)
        .ok_or_else(|| "LLM response missing <updated_file> block".to_string())?;

    astra_skills::improvement::apply_improvement(&skill_md, &new_content)
        .map_err(|e| format!("failed to write {}: {}", skill_md.display(), e))?;

    let proposal = astra_skills::improvement::ImprovementProposal {
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

/// Fields extracted from HeavyCheckpoint for CSL persistence.
/// `None` on any field means "no data available, preserve previous CSL value".
/// For nullable fields (approval_overrides, delegation, compaction_tracker,
/// interruption): `Some(None)` = explicitly cleared, `Some(Some(v))` = new value.
struct CslCheckpointFields {
    blocked_tools: Option<Vec<String>>,
    approval_overrides: Option<Option<serde_json::Value>>,
    budget_remaining_tokens: Option<u64>,
    budget_remaining_rounds: Option<u32>,
    consecutive_ctx_errors: Option<u32>,
    interruption: Option<Option<serde_json::Value>>,
    delegation: Option<Option<astra_turn_core::conversation_log::DelegationCompact>>,
    compaction_tracker: Option<Option<serde_json::Value>>,
}

fn extract_csl_fields_from_result(result: &StreamResult) -> CslCheckpointFields {
    if let Some(astra_pipeline::step_protocol::StepCheckpoint::Heavy(ref heavy)) =
        result.last_heavy_checkpoint
    {
        let delegation = match (&heavy.delegation_id, &heavy.delegation_pattern) {
            (Some(id), Some(pattern)) => {
                Some(astra_turn_core::conversation_log::DelegationCompact {
                    id: id.clone(),
                    pattern: pattern.clone(),
                    completed_sub_runs: heavy.delegation_sub_run_summaries.clone(),
                })
            }
            _ => None,
        };
        CslCheckpointFields {
            blocked_tools: Some(heavy.blocked_tools.clone()),
            approval_overrides: Some(heavy.approval_overrides.clone()),
            budget_remaining_tokens: Some(heavy.budget_remaining_tokens),
            budget_remaining_rounds: Some(heavy.budget_remaining_rounds),
            consecutive_ctx_errors: Some(heavy.consecutive_context_window_errors),
            interruption: Some(heavy.interruption.clone()),
            delegation: Some(delegation),
            compaction_tracker: Some(heavy.compaction_state.clone()),
        }
    } else {
        // No HeavyCheckpoint: all fields fall back to prev_state.
        // interruption from StreamResult is NOT authoritative here —
        // it's only populated by the agentic loop which also writes
        // a HeavyCheckpoint, so if there's no checkpoint, interruption
        // should be preserved from the previous CSL state too.
        CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        }
    }
}

/// Build a full `SessionStateCompact` from REPL state, checkpoint fields, and
/// the previous CSL state. Fields from `cp` that are `None` fall back to
/// `prev_state`, so the no-checkpoint path preserves previously persisted values.
fn build_full_session_state_compact(
    state: &ReplState,
    cp: CslCheckpointFields,
    prev_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        recent_tools: state.recent_tools.clone(),
        continuity: state.runtime_continuity.clone(),
        blocked_tools: cp
            .blocked_tools
            .unwrap_or_else(|| prev_state.blocked_tools.clone()),
        approval_overrides: cp
            .approval_overrides
            .unwrap_or_else(|| prev_state.approval_overrides.clone()),
        budget_remaining_tokens: cp
            .budget_remaining_tokens
            .unwrap_or(prev_state.budget_remaining_tokens),
        budget_remaining_rounds: cp
            .budget_remaining_rounds
            .unwrap_or(prev_state.budget_remaining_rounds),
        consecutive_ctx_errors: cp
            .consecutive_ctx_errors
            .unwrap_or(prev_state.consecutive_ctx_errors),
        interruption: cp
            .interruption
            .unwrap_or_else(|| prev_state.interruption.clone()),
        delegation: cp
            .delegation
            .unwrap_or_else(|| prev_state.delegation.clone()),
        compaction_tracker: cp
            .compaction_tracker
            .unwrap_or_else(|| prev_state.compaction_tracker.clone()),
    }
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
        .filter(|m| matches!(m.source, astra_skills::manifest::SkillSourceKind::Local))
        .collect();

    if filesystem_skills.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    // Build recent messages for analysis
    let recent: Vec<astra_skills::improvement::RecentMessage> = state
        .history
        .iter()
        .rev()
        .take(astra_skills::improvement::TURN_BATCH_SIZE as usize)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                astra_skills::improvement::RecentMessage {
                    role: "user".into(),
                    content: user.clone(),
                },
                astra_skills::improvement::RecentMessage {
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

    let improvements: Vec<astra_skills::improvement::SkillImprovement> = corrections
        .iter()
        .map(|c| {
            let snippet: String = c.chars().take(240).collect();
            astra_skills::improvement::SkillImprovement {
                section: "Recent user feedback".into(),
                change: format!("User correction: {}", snippet),
                reason: "Detected correction pattern in user message".into(),
            }
        })
        .collect();

    let proposal = astra_skills::improvement::ImprovementProposal {
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
    if let Err(e) = astra_skills::improvement::apply_improvement(&skill_md, &new_content) {
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
            serde_json::from_str::<astra_config::runtime_config::RuntimeConfig>(json)
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
    let existing = astra_pipeline::step_checkpoint::list_checkpoints(sid)
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
) -> astra_pipeline::step_protocol::StepCheckpoint {
    use astra_pipeline::step_protocol::{
        ExecutionCursor, HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint,
        epoch_ms,
    };

    let mut messages = Vec::new();
    for (u, a) in &state.history {
        messages.push(serde_json::json!({ "role": "user", "content": u }));
        messages.push(serde_json::json!({ "role": "assistant", "content": a }));
    }

    let max_turns = std::env::var("ASTRA_CLI_MAX_TURNS")
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
        continuity_state: None,
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
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
) -> Result<std::path::PathBuf, String> {
    use astra_pipeline::step_checkpoint::{
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
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
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
    let svc = std::sync::Arc::clone(mc.sync_service());
    let svc2 = std::sync::Arc::clone(&svc);
    let sid_owned = sid.to_string();
    let cp_clone = session_cp.clone();
    mc.spawn_session_sync_task(async move {
        if let Err(e) = svc.push_checkpoint(&sid_owned, &user_id, &cp_clone).await {
            astra_core::agent_warn!("checkpoint", "cloud push session checkpoint: {e}");
        }
    });

    let sid_step = sid.to_string();
    let title_owned = title.to_string();
    let state_json = serde_json::to_string(step_cp).unwrap_or_default();
    let tools_json =
        serde_json::to_string(&state.recent_tools).unwrap_or_else(|_| "[]".to_string());
    mc.spawn_session_sync_task(async move {
        if let Err(e) = svc2
            .push_step_checkpoint(
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_pipeline::step_checkpoint::read_composite_snapshot_index;
    use astra_pipeline::step_protocol::StepCheckpoint;

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
            .push(astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
                turn_id: "turn-3".into(),
                tools: astra_turn_core::context_assembly_trace::ToolSelectionTrace {
                    selection_strategy: "code-intel".into(),
                    selection_confidence: 0.92,
                    tools_selected: vec![astra_turn_core::context_assembly_trace::ToolSelected {
                        tool_name: "lsp".into(),
                        score: 1.0,
                        tokens: 0,
                        selection_factors: Vec::new(),
                    }],
                    ..Default::default()
                },
                memory: astra_turn_core::context_assembly_trace::MemoryRetrievalTrace {
                    query: "resume trace persistence".into(),
                    memories_selected: vec![astra_turn_core::context_assembly_trace::MemorySelection {
                        memory_id: "m1".into(),
                        memory_type: "semantic".into(),
                        content_preview: "trace".into(),
                        relevance_score: 0.8,
                        tokens: 10,
                        source: astra_turn_core::context_assembly_trace::MemorySource::Memoria,
                    }],
                    ..Default::default()
                },
                history: astra_turn_core::context_assembly_trace::HistorySelectionTrace {
                    turns_compressed: vec![astra_turn_core::context_assembly_trace::TurnCompression {
                        turn_index: 1,
                        role: "assistant".into(),
                        original_tokens: 100,
                        compressed_tokens: 50,
                        compression_method:
                            astra_turn_core::context_assembly_trace::CompressionMethod::ReactiveCompact,
                        information_lost: Vec::new(),
                    }],
                    compression_ratio: 0.5,
                    tokens_before: 100,
                    tokens_after: 50,
                    ..Default::default()
                },
                token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                    max_tokens: 16_000,
                    total_used: 8_200,
                    budget_pressure: 0.76,
                    ..Default::default()
                },
                explanations: vec![astra_turn_core::context_assembly_trace::DecisionExplanation {
                    decision_type:
                        astra_turn_core::context_assembly_trace::DecisionType::StrategyChoice {
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
                .tool_selection
                .as_ref()
                .map(|selection| selection.selection_scope.as_str()),
            Some("latest_round")
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
                selection_scope: "latest_round".into(),
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
    fn maybe_update_session_goal_promotes_substantive_goal_after_greeting() {
        let mut state = ReplState::default();

        maybe_update_session_goal(&mut state, "hi");
        assert!(state.session_goal.is_none());

        maybe_update_session_goal(&mut state, "review local changes");
        assert_eq!(state.session_goal.as_deref(), Some("review local changes"));
    }

    #[test]
    fn maybe_update_session_goal_replaces_placeholder_but_preserves_real_goal() {
        let mut state = ReplState {
            session_goal: Some("hi".to_string()),
            ..ReplState::default()
        };

        maybe_update_session_goal(&mut state, "review local changes");
        assert_eq!(state.session_goal.as_deref(), Some("review local changes"));

        maybe_update_session_goal(&mut state, "investigate auth refresh drift");
        assert_eq!(state.session_goal.as_deref(), Some("review local changes"));
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
        // Denominator = full billable input (fresh + cache-read + cache-creation).
        let prompt = 200u64;
        let cache_read = 800u64;
        let cache_creation = 0u64;
        let total_input = prompt + cache_read + cache_creation;
        let cache_pct = cache_read as f64 / total_input.max(1) as f64 * 100.0;
        assert!((cache_pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_zero_when_no_cache() {
        let prompt = 1000u64;
        let cache_read = 0u64;
        let cache_creation = 0u64;
        let total_input = prompt + cache_read + cache_creation;
        let cache_pct = cache_read as f64 / total_input.max(1) as f64 * 100.0;
        assert!((cache_pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_with_heavy_cache_creation() {
        // Regression guard: cache_creation-heavy turn must NOT report 100% hit
        // rate. Before the denominator fix, a turn with fresh=12, cache_read=29816,
        // cache_creation=38788 reported cache:100% (29816 / (12+29816) = 99.96%).
        let prompt = 12u64;
        let cache_read = 29_816u64;
        let cache_creation = 38_788u64;
        let total_input = prompt + cache_read + cache_creation;
        let cache_pct = cache_read as f64 / total_input.max(1) as f64 * 100.0;
        assert!(
            (cache_pct - 43.5).abs() < 1.0,
            "expected ~43.5%, got {cache_pct:.1}%"
        );
    }

    #[test]
    fn cache_hit_percentage_100_only_when_all_input_was_cache_read() {
        let prompt = 0u64;
        let cache_read = 5000u64;
        let cache_creation = 0u64;
        let total_input = prompt + cache_read + cache_creation;
        let cache_pct = cache_read as f64 / total_input.max(1) as f64 * 100.0;
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
            runtime_continuity: Default::default(),
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
            interruption: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
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
    fn analyze_repl_turn_learning_flags_llm_round_churn() {
        let llm_round_event = |round: u32, tokens_in: u64| {
            let mut event = session_journal::JournalEvent::base_public(
                session_journal::JournalEventType::LlmRound,
                Some("sess-1"),
            );
            event.turn = Some(2);
            event.round = Some(round);
            event.tokens_in = Some(tokens_in);
            event.metadata = Some(serde_json::json!({
                "source": "agentic_loop",
            }));
            event
        };
        let mut result = stub_stream_result("");
        result.tools_used = vec!["git_diff".into()];
        result.tool_calls_count = 1;
        result.prompt_tokens = 136_947;
        result.llm_rounds = Some(9);
        result.turn_observability_events =
            vec![llm_round_event(0, 9_401), llm_round_event(7, 20_954)];
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git_diff".into(),
            ok: true,
            ms: 12,
            error: None,
            input_bytes: Some(16),
            output_bytes: Some(240),
            args_preview: None,
            result_preview: Some("diff".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let learning = analyze_repl_turn_learning("review local changes", 2, &[], &result);
        assert!(
            learning.eval.signals.iter().any(|signal| matches!(
                signal,
                astra_runtime::pipeline::evaluation::EvalSignal::LlmRoundChurn {
                    rounds: 9,
                    prompt_tokens: 136_947,
                }
            )),
            "expected llm_round_churn signal, got {:?}",
            learning.eval.signals
        );
        assert!(
            learning.eval.signals.iter().any(|signal| matches!(
                signal,
                astra_runtime::pipeline::evaluation::EvalSignal::PromptGrowthChurn {
                    first_prompt_tokens: 9_401,
                    max_prompt_tokens: 20_954,
                    delta_tokens: 11_553,
                }
            )),
            "expected prompt_growth_churn signal, got {:?}",
            learning.eval.signals
        );
        assert!(
            !learning.eval.signals.iter().any(|signal| matches!(
                signal,
                astra_runtime::pipeline::evaluation::EvalSignal::AllToolsHealthy
            )),
            "llm-round churn must revoke all_tools_healthy: {:?}",
            learning.eval.signals
        );
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
    fn interrupted_success_turn_is_marked_partial_in_journal() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-turn-partial-{}", uuid::Uuid::new_v4());
        let mut state = ReplState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 7,
            ..Default::default()
        };
        let mut result = stub_stream_result(
            "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message.",
        );
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 3,
            "user_message": "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message."
        }));
        result.tool_calls_count = 3;

        let learning = analyze_repl_turn_learning("continue", state.turn, &[], &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "continue",
            &result,
            &learning,
            Instant::now(),
        );

        let event = state.last_turn_event.as_ref().expect("turn event");
        let metadata = event.metadata.as_ref().expect("partial metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interrupted"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);

        let events = session_journal::read_journal(&sid).unwrap();
        let persisted = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("persisted turn event");
        assert_eq!(persisted.metadata.as_ref().unwrap()["partial"], true);
    }

    #[test]
    fn interruption_metadata_preserves_non_object_previous_metadata() {
        let interruption = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
        });

        let metadata =
            merge_interruption_metadata(Some(serde_json::json!("legacy-metadata")), &interruption);

        assert_eq!(metadata["previous_metadata"], "legacy-metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);
    }

    #[test]
    fn interrupted_turn_replay_persists_observability_and_context_trace() {
        let (_tmp, _g) = isolated_sessions_dir();
        let sid = format!("test-turn-replay-{}", uuid::Uuid::new_v4());
        let mut state = ReplState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 4,
            ..Default::default()
        };
        let partial_text =
            "[budget_exhausted] 2 tool call(s) completed. You can continue in the next message.";
        let mut result = stub_stream_result(partial_text);
        result.prompt_tokens = 12_345;
        result.completion_tokens = 234;
        result.llm_rounds = Some(2);
        result.tool_calls_count = 2;
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 2,
            "user_message": partial_text,
        }));
        let mut llm_round = session_journal::JournalEvent::base_public(
            session_journal::JournalEventType::LlmRound,
            Some(&sid),
        );
        llm_round.turn = Some(4);
        llm_round.round = Some(1);
        llm_round.tokens_in = Some(12_345);
        llm_round.tokens_out = Some(234);
        llm_round.metadata = Some(serde_json::json!({
            "source": "agentic_loop",
            "finish_reason": "tool_calls",
        }));
        result.turn_observability_events = vec![llm_round];
        result.pending_context_assembly_trace = Some((
            99,
            serde_json::json!({
                "turn_id": "turn-99",
                "tools": {
                    "tools_selected": [
                        {"tool_name": "git_diff"},
                        {"tool_name": "read_file"}
                    ]
                },
                "token_budget": {"total_used": 12_345}
            }),
        ));

        let learning = analyze_repl_turn_learning("continue", state.turn, &[], &result);
        commit_turn_journal_workspace_and_sidecars(
            &mut state,
            "continue",
            &result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let llm_round = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::LlmRound)
            .expect("persisted llm_round event");
        assert_eq!(llm_round.turn, Some(4));
        assert_eq!(llm_round.round, Some(1));
        assert_eq!(
            llm_round.metadata.as_ref().unwrap()["source"],
            "agentic_loop"
        );

        let turn_event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("persisted turn event");
        assert_eq!(turn_event.turn, Some(4));
        assert_eq!(turn_event.assistant_output.as_deref(), Some(partial_text));
        let metadata = turn_event.metadata.as_ref().expect("turn metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["tool_calls_completed"], 2);

        let assembly_event = events
            .iter()
            .find(|event| {
                event.event_type == session_journal::JournalEventType::ContextAssemblyRecorded
            })
            .expect("persisted context assembly event");
        assert_eq!(assembly_event.turn, Some(4));
        assert_eq!(
            assembly_event.metadata.as_ref().unwrap()["trace_recorded"],
            true
        );
        assert_eq!(
            assembly_event.metadata.as_ref().unwrap()["turn_id"],
            "turn-99"
        );
        assert_eq!(assembly_event.metadata.as_ref().unwrap()["tool_count"], 2);
        assert_eq!(
            assembly_event.metadata.as_ref().unwrap()["total_tokens"],
            12_345
        );
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
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
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
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
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
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "hello, please add a feature".to_string(),
                "sure thing".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
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
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, don't greet twice — skip the greeting on follow-ups".to_string(),
                "Hello again!".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
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
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![("no, that's wrong".to_string(), "sorry".to_string())],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
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
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = ReplState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, that's wrong, do it differently".to_string(),
                "sorry".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
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

    // ── tool summary for cross-turn context ───────────────────────────────────

    fn make_record(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            file_path: file_path.map(|s| s.into()),
            ..Default::default()
        }
    }

    #[test]
    fn tool_summary_empty_when_no_tools() {
        let summary = super::build_turn_tool_summary(&[]);
        assert!(summary.is_empty());
    }

    #[test]
    fn tool_summary_lists_files_touched() {
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("str_replace", true, Some("src/main.rs")),
            make_record("read_file", true, Some("src/lib.rs")),
        ];
        let summary = super::build_turn_tool_summary(&records);
        assert!(
            summary.contains("src/main.rs"),
            "should list files: {summary}"
        );
        assert!(
            summary.contains("src/lib.rs"),
            "should list files: {summary}"
        );
        // Deduped
        assert_eq!(
            summary.matches("src/main.rs").count(),
            1,
            "should dedup: {summary}"
        );
    }

    #[test]
    fn tool_summary_shows_failed_tools() {
        let records = vec![
            make_record("read_file", false, Some("src/missing.rs")),
            make_record("bash", true, None),
        ];
        let summary = super::build_turn_tool_summary(&records);
        assert!(
            summary.contains("read_file") && summary.contains("fail"),
            "should show failures: {summary}"
        );
    }

    #[test]
    fn tool_summary_appended_to_history() {
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("str_replace", true, Some("src/main.rs")),
        ];
        let full_text = "## Done\nFixed the bug.".to_string();
        let history_text = super::build_history_text(&full_text, &records);
        assert!(
            history_text.starts_with("## Done"),
            "original text preserved"
        );
        assert!(
            history_text.contains("src/main.rs"),
            "summary appended: {history_text}"
        );
    }

    #[test]
    fn tool_summary_not_appended_when_no_tools() {
        let full_text = "Just a text response.".to_string();
        let history_text = super::build_history_text(&full_text, &[]);
        assert_eq!(history_text, full_text, "no summary when no tools");
    }

    // ── multi-turn simulation: real session scenarios ─────────────────────────

    /// Simulate the exact scenario from session 15a5eb62 (glm-5.1):
    /// Turn 1: simple chat (no tools)
    /// Turn 2: code review with skill + git_diff + read_file (3 tool calls)
    /// Turn 3: "修复和优化" — model needs paths from Turn 2
    ///
    /// BEFORE fix: Turn 3 prompt had zero file paths from Turn 2.
    /// AFTER fix: Turn 3 prompt should contain file paths from Turn 2's tool summary.
    #[test]
    fn multi_turn_path_continuity_review_then_fix() {
        let mut history: Vec<(String, String)> = Vec::new();

        // ── Turn 1: simple chat ──
        let t1_text = "I can help you with that project.";
        let t1_records: Vec<session_journal::ToolCallRecord> = vec![];
        history.push((
            "hello".into(),
            super::build_history_text(t1_text, &t1_records),
        ));

        // ── Turn 2: code review (skill + git_diff + read_file) ──
        let t2_text = "## Code Review\n\n**permission_manager.rs:978** — boundary check incomplete\n**safety_middleware.rs:8** — missing UPDATE keyword\n**journal_digest.rs:241** — use enum instead of String";
        let t2_records = vec![
            make_record("skill", true, None),
            make_record("git_diff", true, None),
            make_record("git_diff", true, None),
            make_record(
                "read_file",
                true,
                Some("rust/crates/astra-cli/src/cli/permission_manager.rs"),
            ),
            make_record(
                "read_file",
                true,
                Some("rust/crates/astra-turn-core/src/safety_middleware.rs"),
            ),
            make_record(
                "read_file",
                true,
                Some("rust/crates/astra-cli/src/cli/journal_digest.rs"),
            ),
            make_record("grep", true, None),
        ];
        history.push((
            "review latest commit".into(),
            super::build_history_text(t2_text, &t2_records),
        ));

        // ── Turn 3: model sees the prompt ──
        let messages = super::history_as_messages(&history);
        // Add current user message
        let current = "修复和优化";
        let mut full_messages = messages;
        full_messages.push(serde_json::json!({"role": "user", "content": current}));

        // The assistant message from Turn 2 should contain the tool summary
        let t2_assistant = full_messages[3]["content"].as_str().unwrap();

        // CRITICAL: model can now see the full file paths from Turn 2
        assert!(
            t2_assistant.contains("rust/crates/astra-cli/src/cli/permission_manager.rs"),
            "Turn 3 prompt must contain permission_manager.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );
        assert!(
            t2_assistant.contains("rust/crates/astra-turn-core/src/safety_middleware.rs"),
            "Turn 3 prompt must contain safety_middleware.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );
        assert!(
            t2_assistant.contains("rust/crates/astra-cli/src/cli/journal_digest.rs"),
            "Turn 3 prompt must contain journal_digest.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );

        // The review text only had short names — the summary provides full paths
        assert!(
            !t2_text.contains("rust/crates/"),
            "review text itself should NOT have full paths (that's the whole problem)"
        );
    }

    /// Simulate 4 turns with increasing complexity:
    /// Turn 1: review (7 tools, 3 files)
    /// Turn 2: fix (18 tools, 3 files, 2 failures)
    /// Turn 3: review changes (3 tools)
    /// Turn 4: optimize (7 tools, 1 file)
    /// Verify Turn 4 can see all prior file paths.
    #[test]
    fn four_turn_session_context_accumulation() {
        let mut history: Vec<(String, String)> = Vec::new();

        // Turn 1
        let t1_records = vec![
            make_record("skill", true, None),
            make_record("git_diff", true, None),
            make_record("read_file", true, Some("src/cli/permission_manager.rs")),
            make_record("read_file", false, Some("src/safety_middleware.rs")),
            make_record("grep", true, None),
        ];
        history.push((
            "review latest commit".into(),
            super::build_history_text(
                "## Review\nIssues found in permission_manager.rs",
                &t1_records,
            ),
        ));

        // Turn 2
        let t2_records = vec![
            make_record("read_file", true, Some("src/cli/permission_manager.rs")),
            make_record("read_file", true, Some("src/safety_middleware.rs")),
            make_record("str_replace", true, Some("src/cli/permission_manager.rs")),
            make_record("str_replace", true, Some("src/safety_middleware.rs")),
            make_record("str_replace", true, Some("src/cli/journal_digest.rs")),
            make_record("bash", true, None),
            make_record("bash", false, None),
        ];
        history.push((
            "修复和优化".into(),
            super::build_history_text("## Done\nFixed 3 files.", &t2_records),
        ));

        // Turn 3
        let t3_records = vec![
            make_record("skill", true, None),
            make_record("git_diff", true, None),
        ];
        history.push((
            "review changes".into(),
            super::build_history_text(
                "## Review\nLGTM. Suggest adding Default to ErrorCategory.",
                &t3_records,
            ),
        ));

        // Turn 4 prompt
        let messages = super::history_as_messages(&history);
        let mut full_messages = messages;
        full_messages.push(serde_json::json!({"role": "user", "content": "按照建议优化"}));

        // Turn 4 should see file paths from ALL prior turns
        let all_text: String = full_messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // From Turn 1
        assert!(
            all_text.contains("src/cli/permission_manager.rs"),
            "Turn 1 path visible"
        );
        // From Turn 2
        assert!(
            all_text.contains("src/cli/journal_digest.rs"),
            "Turn 2 path visible"
        );
        assert!(
            all_text.contains("src/safety_middleware.rs"),
            "Turn 2 path visible"
        );
        // Turn 1 failure visible
        assert!(
            all_text.contains("failed: read_file"),
            "Turn 1 failure visible"
        );
        // Turn 2 failure visible
        assert!(all_text.contains("failed: bash"), "Turn 2 failure visible");
    }

    /// Verify the summary doesn't bloat context excessively.
    /// With 20 unique files across 50 tool calls, summary should be < 2KB.
    /// Files beyond 15 are truncated with "(+N more)".
    #[test]
    fn tool_summary_stays_compact_under_heavy_load() {
        let mut records = Vec::new();
        for i in 0..50 {
            let file = format!("src/module_{}/file_{}.rs", i / 5, i % 5);
            records.push(make_record(
                if i % 3 == 0 {
                    "read_file"
                } else {
                    "str_replace"
                },
                i % 7 != 0,
                Some(&file),
            ));
        }
        let summary = super::build_turn_tool_summary(&records);
        assert!(
            summary.len() < 2048,
            "summary should be compact, got {} bytes: {summary}",
            summary.len()
        );
        // First 15 files shown
        assert!(summary.contains("src/module_0/file_0.rs"));
        // Truncation indicator
        assert!(
            summary.contains("more)"),
            "should truncate beyond 15 files: {summary}"
        );
    }

    /// Verify that compaction + tool summary works together.
    /// After compaction, the summary from the compacted turn should still be
    /// in the compacted summary text (since it's part of the assistant text).
    #[test]
    fn tool_summary_survives_in_compacted_history() {
        let mut history: Vec<(String, String)> = Vec::new();

        // Simulate a compacted entry (empty user = compacted)
        history.push((
            String::new(),
            "[Prior context — 3 turns compacted]\nUser worked on fixing permission_manager.rs and safety_middleware.rs.\n\n[Turn context: files: src/permission_manager.rs, src/safety_middleware.rs | tool_calls: 12]".into(),
        ));

        // Recent turn
        let records = vec![
            make_record("read_file", true, Some("src/journal_digest.rs")),
            make_record("str_replace", true, Some("src/journal_digest.rs")),
        ];
        history.push((
            "add Default derive".into(),
            super::build_history_text("Added #[derive(Default)] to ErrorCategory.", &records),
        ));

        let messages = super::history_as_messages(&history);
        let mut full = messages;
        full.push(serde_json::json!({"role": "user", "content": "run tests"}));

        let all_text: String = full
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // Compacted summary preserves old file paths
        assert!(
            all_text.contains("src/permission_manager.rs"),
            "compacted paths visible"
        );
        // Recent turn has new paths
        assert!(
            all_text.contains("src/journal_digest.rs"),
            "recent paths visible"
        );
    }

    // ── CSL persistence tests ────────────────────────────────────────────
    // These tests now exercise CslManager directly (the unified abstraction).
    // The old `persist_csl_after_turn` function was deleted in the Phase 2 refactor.

    #[tokio::test]
    async fn csl_first_turn_writes_snapshot_and_advances_seq() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = isolated_sessions_dir();
        let session_id = format!("csl-first-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        let full_messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];

        let mut session_state = SessionStateCompact::default();
        session_state.recent_tools = vec!["bash".into()];

        mgr.persist_turn(1, &full_messages, &session_state)
            .await
            .unwrap();

        assert!(mgr.last_seq() > 0, "seq should advance after first turn");

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        assert!(
            !entries.is_empty(),
            "should have written at least one entry"
        );
        assert!(entries[0].is_snapshot(), "first entry must be a Snapshot");

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.session_state.recent_tools, vec!["bash".to_string()]);
    }

    #[tokio::test]
    async fn csl_subsequent_turn_writes_delta_not_snapshot() {
        use astra_turn_core::conversation_log::{
            CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = isolated_sessions_dir();
        let session_id = format!("csl-delta-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        let t1_msgs = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
        ];
        mgr.persist_turn(1, &t1_msgs, &SessionStateCompact::default())
            .await
            .unwrap();
        let seq_after_t1 = mgr.last_seq();

        mgr.mark_turn_start(t1_msgs.len());
        let t2_full = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
            serde_json::json!({"role": "user", "content": "q2"}),
            serde_json::json!({"role": "assistant", "content": "a2"}),
        ];
        mgr.persist_turn(2, &t2_full, &SessionStateCompact::default())
            .await
            .unwrap();
        let seq_after_t2 = mgr.last_seq();
        assert!(
            seq_after_t2 > seq_after_t1,
            "seq should advance: t1={seq_after_t1}, t2={seq_after_t2}"
        );

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let snapshot_count = entries.iter().filter(|e| e.is_snapshot()).count();
        let delta_count = entries
            .iter()
            .filter(|e| matches!(e, CslEntry::TurnDelta { .. }))
            .count();
        assert_eq!(snapshot_count, 1, "should have exactly 1 snapshot");
        assert_eq!(delta_count, 1, "should have exactly 1 delta");

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 4);
        assert_eq!(mat.messages[2]["content"], "q2");
        assert_eq!(mat.messages[3]["content"], "a2");
    }

    #[tokio::test]
    async fn csl_periodic_snapshot_every_5_turns() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = isolated_sessions_dir();
        let session_id = format!("csl-snap5-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=5u32 {
            let full: Vec<serde_json::Value> = (1..=t)
                .map(|i| serde_json::json!({"role": "user", "content": format!("turn {i}")}))
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { (t - 1) as usize });
            mgr.persist_turn(t, &full, &SessionStateCompact::default())
                .await
                .unwrap();
        }

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        assert_eq!(entries.len(), 1, "only the latest snapshot should remain");
        assert!(entries[0].is_snapshot());
        assert_eq!(entries[0].turn(), 5);

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 5, "snapshot should contain all 5 turns");
        assert_eq!(mat.messages[4]["content"], "turn 5");

        assert_eq!(mgr.last_seq(), 6);

        let all_entries = store.load_after(&session_id, 0).await.unwrap();
        let total_snapshots = all_entries.iter().filter(|e| e.is_snapshot()).count();
        assert_eq!(
            total_snapshots, 2,
            "should have initial + periodic snapshot"
        );
    }

    #[tokio::test]
    async fn csl_persist_and_resume_roundtrip() {
        use astra_turn_core::conversation_log::{
            SessionStateCompact, file_store::FileCslStore, manager::CslManager,
        };

        let (_tmp, _guard) = isolated_sessions_dir();
        let session_id = format!("csl-rt-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=3u32 {
            let mut session_state = SessionStateCompact::default();
            session_state.recent_tools = vec![format!("tool_{t}")];
            let full: Vec<serde_json::Value> = (1..=t)
                .flat_map(|i| {
                    vec![
                        serde_json::json!({"role": "user", "content": format!("q{i}")}),
                        serde_json::json!({"role": "assistant", "content": format!("a{i}")}),
                    ]
                })
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { ((t - 1) * 2) as usize });
            mgr.persist_turn(t, &full, &session_state).await.unwrap();
        }

        let saved_seq = mgr.last_seq();

        // Resume from CSL in fresh manager
        let mut mgr2 = CslManager::new(store, session_id.clone(), Default::default()).unwrap();
        let mat = mgr2.load().await.unwrap().expect("should have entries");

        assert_eq!(mat.messages.len(), 6, "3 turns × 2 messages");
        assert_eq!(mat.messages[0]["content"], "q1");
        assert_eq!(mat.messages[5]["content"], "a3");
        assert_eq!(mat.last_seq, saved_seq);
        assert_eq!(
            mat.session_state.recent_tools,
            vec!["tool_3".to_string()],
            "should have last turn's recent_tools"
        );
    }

    #[tokio::test]
    async fn csl_undo_resets_seq_and_next_turn_writes_fresh_snapshot() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = isolated_sessions_dir();
        let session_id = format!("csl-undo-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=2u32 {
            let full: Vec<serde_json::Value> = (1..=t)
                .map(|i| serde_json::json!({"role": "user", "content": format!("q{i}")}))
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { (t - 1) as usize });
            mgr.persist_turn(t, &full, &SessionStateCompact::default())
                .await
                .unwrap();
        }
        assert!(mgr.last_seq() > 0, "seq should be > 0 after 2 turns");

        mgr.reset().await.unwrap();
        assert_eq!(mgr.last_seq(), 0, "seq should be 0 after reset");

        let post_undo_msgs = vec![serde_json::json!({"role": "user", "content": "after-undo"})];
        mgr.persist_turn(2, &post_undo_msgs, &SessionStateCompact::default())
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 1, "fresh snapshot should have 1 msg");
        assert_eq!(mat.messages[0]["content"], "after-undo");
    }

    // ── No-checkpoint path: must preserve prev_state fields ─────────

    #[test]
    fn no_checkpoint_preserves_blocked_tools_from_prev_state() {
        let state = &ReplState {
            recent_tools: vec!["read_file".into()],
            ..Default::default()
        };
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["bash".into(), "write".into()],
            ..Default::default()
        };

        // Simulate no-checkpoint path: all Option fields are None,
        // blocked_tools would currently be Vec::new() — the bug.
        let cp = CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(
            result.blocked_tools,
            vec!["bash", "write"],
            "no-checkpoint path must preserve blocked_tools from prev_state"
        );
    }

    #[test]
    fn no_checkpoint_preserves_approval_overrides_from_prev_state() {
        let state = &ReplState::default();
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            approval_overrides: Some(serde_json::json!({"tool": "bash", "approved": true})),
            ..Default::default()
        };

        let cp = CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(
            result.approval_overrides,
            Some(serde_json::json!({"tool": "bash", "approved": true})),
            "no-checkpoint path must preserve approval_overrides from prev_state"
        );
    }

    #[test]
    fn no_checkpoint_preserves_delegation_from_prev_state() {
        let state = &ReplState::default();
        let delegation = astra_turn_core::conversation_log::DelegationCompact {
            id: "d1".into(),
            pattern: "review".into(),
            completed_sub_runs: vec![],
        };
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            delegation: Some(delegation.clone()),
            ..Default::default()
        };

        let cp = CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(
            result.delegation,
            Some(delegation),
            "no-checkpoint path must preserve delegation from prev_state"
        );
    }

    #[test]
    fn no_checkpoint_preserves_compaction_tracker_from_prev_state() {
        let state = &ReplState::default();
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            compaction_tracker: Some(serde_json::json!({"version": 3})),
            ..Default::default()
        };

        let cp = CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(
            result.compaction_tracker,
            Some(serde_json::json!({"version": 3})),
            "no-checkpoint path must preserve compaction_tracker from prev_state"
        );
    }

    #[test]
    fn checkpoint_path_overrides_prev_state() {
        let state = &ReplState {
            recent_tools: vec!["exec".into()],
            ..Default::default()
        };
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["old_bash".into()],
            approval_overrides: Some(serde_json::json!({"old": true})),
            delegation: Some(astra_turn_core::conversation_log::DelegationCompact {
                id: "old_d".into(),
                pattern: "old_p".into(),
                completed_sub_runs: vec![],
            }),
            compaction_tracker: Some(serde_json::json!({"old": 1})),
            budget_remaining_tokens: 99_999,
            budget_remaining_rounds: 99,
            consecutive_ctx_errors: 99,
            ..Default::default()
        };

        let cp = CslCheckpointFields {
            blocked_tools: Some(vec!["new_bash".into()]),
            approval_overrides: Some(Some(serde_json::json!({"new": true}))),
            budget_remaining_tokens: Some(50_000),
            budget_remaining_rounds: Some(5),
            consecutive_ctx_errors: Some(1),
            interruption: None,
            delegation: Some(Some(astra_turn_core::conversation_log::DelegationCompact {
                id: "new_d".into(),
                pattern: "new_p".into(),
                completed_sub_runs: vec![],
            })),
            compaction_tracker: Some(Some(serde_json::json!({"new": 2}))),
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(result.blocked_tools, vec!["new_bash"]);
        assert_eq!(
            result.approval_overrides,
            Some(serde_json::json!({"new": true}))
        );
        assert_eq!(result.budget_remaining_tokens, 50_000);
        assert_eq!(result.budget_remaining_rounds, 5);
        assert_eq!(result.consecutive_ctx_errors, 1);
        assert_eq!(result.delegation.unwrap().id, "new_d");
        assert_eq!(
            result.compaction_tracker,
            Some(serde_json::json!({"new": 2}))
        );
    }

    #[test]
    fn checkpoint_explicitly_clears_fields() {
        let state = &ReplState::default();
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["bash".into()],
            approval_overrides: Some(serde_json::json!({"tool": "bash"})),
            delegation: Some(astra_turn_core::conversation_log::DelegationCompact {
                id: "d1".into(),
                pattern: "p1".into(),
                completed_sub_runs: vec![],
            }),
            compaction_tracker: Some(serde_json::json!({"v": 1})),
            ..Default::default()
        };

        // Checkpoint says "explicitly empty/cleared"
        let cp = CslCheckpointFields {
            blocked_tools: Some(vec![]),
            approval_overrides: Some(None),
            budget_remaining_tokens: Some(0),
            budget_remaining_rounds: Some(0),
            consecutive_ctx_errors: Some(0),
            interruption: Some(None),
            delegation: Some(None),
            compaction_tracker: Some(None),
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert!(
            result.blocked_tools.is_empty(),
            "should be explicitly cleared"
        );
        assert!(
            result.approval_overrides.is_none(),
            "should be explicitly cleared"
        );
        assert!(
            result.interruption.is_none(),
            "should be explicitly cleared"
        );
        assert!(result.delegation.is_none(), "should be explicitly cleared");
        assert!(
            result.compaction_tracker.is_none(),
            "should be explicitly cleared"
        );
        assert_eq!(result.budget_remaining_tokens, 0);
    }

    #[test]
    fn no_checkpoint_preserves_interruption_from_prev_state() {
        let state = &ReplState::default();
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            interruption: Some(serde_json::json!({"kind": "budget_exhausted"})),
            ..Default::default()
        };

        let cp = CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        };

        let result = build_full_session_state_compact(state, cp, &prev);
        assert_eq!(
            result.interruption,
            Some(serde_json::json!({"kind": "budget_exhausted"})),
            "no-checkpoint path must preserve interruption from prev_state"
        );
    }

    // ─── P6: should_bootstrap_lessons gate ──────────────────────────────────

    #[test]
    fn should_bootstrap_lessons_true_on_fresh_state() {
        let state = ReplState::default();
        assert!(
            should_bootstrap_lessons(&state),
            "fresh state should bootstrap from Memoria"
        );
    }

    #[test]
    fn should_bootstrap_lessons_skips_when_already_loaded() {
        let mut state = ReplState::default();
        state.session_lessons_loaded = true;
        assert!(
            !should_bootstrap_lessons(&state),
            "loaded flag must prevent re-bootstrap"
        );
    }

    // ─── P8: maybe_run_auto_invoke ──────────────────────────────────────────

    #[tokio::test]
    async fn auto_invoke_noop_when_signals_are_zero() {
        // No observability, no signals → handler should not even be
        // instantiated; latest_skill_diagnosis must stay None.
        let mut state = ReplState::default();
        assert!(state.auto_invoke_handler.is_none());
        assert!(state.latest_skill_diagnosis.is_none());

        maybe_run_auto_invoke(&mut state).await;

        assert!(
            state.auto_invoke_handler.is_none(),
            "zero-signals path must not construct a handler"
        );
        assert!(state.latest_skill_diagnosis.is_none());
    }

    #[tokio::test]
    async fn auto_invoke_populates_latest_diagnosis_on_stall_signal() {
        // Set up an ObservabilitySession with 3 stalls so the gate fires
        // analyze_session. The SyntheticSkillDiagnosisExecutor produces a valid
        // diagnosis block. The run must stash it into
        // state.latest_skill_diagnosis for the next turn.
        let mut state = ReplState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple(
                "p8-turn-end",
            ),
        ));
        {
            let mut g = session.write().unwrap();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
        }
        state.observability_session = Some(session);

        maybe_run_auto_invoke(&mut state).await;

        let diag = state
            .latest_skill_diagnosis
            .as_ref()
            .expect("3 stalls must produce a diagnosis");
        assert_eq!(diag.skill, "analyze_session");
        assert_eq!(diag.cause, "session_stalls");
        // Handler must be cached for reuse next turn (cooldowns persist).
        assert!(state.auto_invoke_handler.is_some());
    }

    #[tokio::test]
    async fn auto_invoke_cooldown_prevents_refire_within_window() {
        // Two consecutive calls with the same stall signal: first fires,
        // second must be silenced by the gate's 60s cooldown. The cached
        // handler is what makes this work — if we re-created it each turn
        // the cooldown state would be lost.
        let mut state = ReplState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple(
                "p8-cooldown",
            ),
        ));
        {
            let mut g = session.write().unwrap();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
        }
        state.observability_session = Some(session);

        maybe_run_auto_invoke(&mut state).await;
        let first_diag = state.latest_skill_diagnosis.clone();
        assert!(first_diag.is_some(), "first turn must fire");

        // Second call: signals unchanged, but cooldown should block.
        // The diagnosis stays visible in the prompt so the LLM knows
        // what it's being measured against while the tracker evaluates.
        maybe_run_auto_invoke(&mut state).await;
        assert!(
            state.latest_skill_diagnosis.is_some(),
            "cooldown must NOT clear the diagnosis — tracker still evaluating"
        );
    }

    // ─── R1: DiagnosisOutcomeTracker wiring ─────────────────────────────────

    #[tokio::test]
    async fn diagnosis_outcome_tracker_accumulates_met_failed() {
        // Setup: session with 3 stalls → auto-invoke fires → diagnosis
        // activated with success_criteria. Then simulate stall count
        // staying flat (criterion "session_stalls_delta <= 0" satisfied).
        let mut state = ReplState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability_integration::ObservabilitySession::new_simple(
                "r1-tracker",
            ),
        ));
        {
            let mut g = session.write().unwrap();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
            g.record_stall_event();
        }
        state.observability_session = Some(session.clone());

        // Turn N: diagnosis fires and is activated in the tracker.
        state.turn = 5;
        maybe_run_auto_invoke(&mut state).await;
        let diag = state
            .latest_skill_diagnosis
            .as_ref()
            .expect("should have fired");
        assert!(
            !diag.success_criteria.is_empty(),
            "synthetic executor must produce criteria"
        );

        // Turn N+1..N+3: signals stay flat (no new stalls), so the
        // "session_stalls_delta <= 0" criterion should become Satisfied
        // once window_turns elapse.
        for t in 6..=10 {
            state.turn = t;
            maybe_run_auto_invoke(&mut state).await;
        }

        // After enough turns, the tracker should have evaluated and
        // accumulated met/failed counts.
        let total = state.diagnosis_criteria_met + state.diagnosis_criteria_failed;
        assert!(
            total > 0,
            "tracker must have evaluated at least one criterion; \
             met={}, failed={}",
            state.diagnosis_criteria_met,
            state.diagnosis_criteria_failed
        );
    }

    #[tokio::test]
    async fn diagnosis_criteria_start_at_zero() {
        let state = ReplState::default();
        assert_eq!(state.diagnosis_criteria_met, 0);
        assert_eq!(state.diagnosis_criteria_failed, 0);
    }
}
