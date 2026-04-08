//! Plan mode interaction handler — extracted from `slash_memory.rs`.
//!
//! Handles all user input while in the interactive plan editing mode (`plan>` prompt).
//! Uses the `PlanCommand` parser for structured commands and falls back to
//! natural-language plan editing via LLM.

use super::*;
use astra_runtime::plan::PlanCommand;
use astra_runtime::plan;
use astra_services::session_journal;
use futures_util::StreamExt;

/// Outcome of collecting text from an SSE stream.
pub(super) struct SseTextResult {
    pub text: String,
    pub event_count: usize,
    pub event_types: Vec<String>,
}

/// Collect text content from an SSE stream response.
pub(super) async fn collect_sse_text(
    resp: reqwest::Response,
    stream_to_stderr: bool,
) -> SseTextResult {
    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = buffer.find("\n\n") {
            let event_str = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            for line in event_str.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    result.event_count += 1;
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = json
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        if !result.event_types.contains(&event_type.to_string()) {
                            result.event_types.push(event_type.to_string());
                        }

                        match event_type {
                            "text_delta" => {
                                if let Some(content) = json.get("content").and_then(|v| v.as_str())
                                {
                                    result.text.push_str(content);
                                    if stream_to_stderr {
                                        eprint!("{}", content);
                                    }
                                }
                            }
                            "error" => {
                                if let Some(msg) = json
                                    .get("message")
                                    .or_else(|| json.get("error"))
                                    .and_then(|v| v.as_str())
                                {
                                    eprintln!("\r  {} Server error: {}", theme::icon_err(), msg);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    for line in buffer.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            result.event_count += 1;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                && json.get("type").and_then(|v| v.as_str()) == Some("text_delta")
                && let Some(content) = json.get("content").and_then(|v| v.as_str())
            {
                result.text.push_str(content);
                if stream_to_stderr {
                    eprint!("{}", content);
                }
            }
        }
    }

    result
}

/// Enrich a `ProjectContext` with learned plan templates from cloud storage.
pub(super) async fn enrich_with_templates(
    context: &mut plan::ProjectContext,
    matrix_runtime: Option<&std::sync::Arc<astra_runtime::MatrixCloudRuntime>>,
    user_id: Option<&str>,
    goal: &str,
    verbose: bool,
) {
    let Some(mc) = matrix_runtime else {
        if verbose {
            eprintln!(
                "  {} No cloud connection — skipping template search",
                "⋯".dim()
            );
        }
        return;
    };
    let pool = mc.shared_pool().get();
    let uid = user_id.unwrap_or("anonymous");

    if verbose {
        eprintln!("  {} Searching for similar plan templates...", "⋯".dim());
    }

    let templates = plan::query_similar_templates(pool, uid, goal, 3).await;
    if !templates.is_empty() {
        eprintln!(
            "  {} Found {} learned template{}",
            "📋".cyan(),
            templates.len(),
            if templates.len() == 1 { "" } else { "s" }
        );
        context.prior_templates = templates;
    } else if verbose {
        eprintln!("  {} No matching templates found", "⋯".dim());
    }
}

pub(super) fn eprint_plan_json_parse_failed(full_text: &str, err: &str) {
    eprintln!("  {} Failed to parse plan: {}", theme::icon_err(), err);
    let prev = plan::plan_response_parse_error_preview(full_text, 10, 700);
    if !prev.is_empty() {
        eprintln!("  {}", "Model reply:".dim());
        for line in prev.lines() {
            eprintln!("    {}", line.dim());
        }
    }
    eprintln!(
        "  {}",
        "Tip: Plan decomposition expects JSON only. For git/files without JSON plans, exit plan mode (`exit`) or `/plan off` and use normal chat with tools."
            .dim()
    );
}

/// Print available plan mode commands (compact, for after plan generation).
pub(super) fn eprint_plan_commands_help() {
    eprintln!("  Type {} for all commands.", "help".cyan());
    eprintln!(
        "  {} {} to run · {} to modify · {} to leave",
        "→".dim(),
        "go".cyan(),
        "<describe changes>".dim(),
        "exit".cyan(),
    );
}

/// Print the full plan mode banner (shown on entry and on `help` command).
pub(super) fn eprint_plan_mode_banner(goal: &str) {
    eprintln!();
    eprintln!("┌─────────────────────────────────────────────────────────────");
    eprintln!("│ {} {}", "📋 PLAN MODE".yellow().bold(), "— interactive plan editor".dim());
    if !goal.is_empty() {
        let display_goal: String = goal.chars().take(50).collect();
        let suffix = if goal.len() > 50 { "…" } else { "" };
        eprintln!("│ Goal: {}{}", display_goal.cyan(), suffix);
    }
    eprintln!("│");
    eprintln!("│ {}", "Quick actions:".bold());
    eprintln!("│   {} / {} / {}   Execute the plan", "go".cyan(), "execute".cyan(), "run".cyan());
    eprintln!("│   {}                 Execute step-by-step", "step".cyan());
    eprintln!("│   {} / {} / {}  Leave plan mode", "exit".cyan(), "quit".cyan(), "cancel".cyan());
    eprintln!("│");
    eprintln!("│ {}", "Inspect:".bold());
    eprintln!("│   {}               Current plan status + progress", "status".cyan());
    eprintln!("│   {}                 Show current plan in detail", "show".cyan());
    eprintln!("│   {}              Plan cost & metrics", "metrics".cyan());
    eprintln!("│   {}             Execution timeline", "timeline".cyan());
    eprintln!("│   {}              Version history", "history".cyan());
    eprintln!("│   {} <from> <to>  Diff between plan versions", "diff".cyan());
    eprintln!("│");
    eprintln!("│ {}", "Edit:".bold());
    eprintln!("│   {}     Rollback to a version", "rollback <ver>".cyan());
    eprintln!("│   {}                 List saved plans", "list".cyan());
    eprintln!("│   {}        Type anything to edit via LLM", "<natural language>".dim());
    eprintln!("│");
    eprintln!("│   {} / {}    Show this help", "help".cyan(), "?".cyan());
    eprintln!("└─────────────────────────────────────────────────────────────");
    eprintln!();
}

/// Write a plan lifecycle journal event.
fn journal_plan_event(
    journal: &mut Option<session_journal::JournalWriter>,
    event_type: session_journal::JournalEventType,
    summary: &str,
    metadata: Option<serde_json::Value>,
) {
    let Some(writer) = journal else {
        return;
    };
    let event = match event_type {
        session_journal::JournalEventType::PlanEdit => {
            session_journal::JournalEvent::plan_edit(None, summary, metadata)
        }
        session_journal::JournalEventType::PlanLifecycle => {
            session_journal::JournalEvent::plan_lifecycle(None, summary, metadata)
        }
        _ => {
            session_journal::JournalEvent::plan_lifecycle(None, summary, metadata)
        }
    };
    let _ = writer.append(&event);
}

/// Cleanly shut down a running plan executor handle.
///
/// Sends `Cancel`, drains remaining updates, and returns `true` if a handle was
/// actually present (i.e. an executor was running). Call this before spawning a
/// new executor or when exiting plan mode.
pub fn shutdown_plan_executor(state: &mut ReplState) -> bool {
    if let Some(mut h) = state.plan_handle.take() {
        let _ = h.send_command(crate::plan_executor::PlanCommand::Cancel);
        while h.try_recv().is_some() {}
        true
    } else {
        false
    }
}

/// True while a plan executor handle is alive (running or paused waiting for Resume).
pub fn plan_execution_ui_active(state: &ReplState) -> bool {
    state.plan_handle.is_some()
}

/// Idle plan status line: "review — not started" when there is a plan and no subtask has started yet.
pub fn plan_idle_review_not_started(ps: &plan::PlanModeState) -> bool {
    use astra_services::task_orchestrator::TaskStatus;
    !ps.plan.subtasks.is_empty()
        && ps
            .plan
            .subtasks
            .iter()
            .all(|s| s.status == TaskStatus::Pending)
}

/// Handle user input while in interactive plan mode (`plan>` prompt).
///
/// This function routes input through the `PlanCommand` parser first. If the
/// input matches a structured command (execute, status, cancel, etc.), the
/// command is dispatched directly. Otherwise, the input is treated as a
/// natural-language plan edit and sent to the LLM.
pub async fn handle_plan_mode_input(
    input: String,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<(), String> {
    use plan::{
        ClarificationAnswer, PlanEntryChoice, PlanModeState,
        decomposition_prompt, format_clarification_question,
        format_plan, parse_clarification_response, parse_plan_entry_choice,
        parse_plan_response,
    };

    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  ⚠️ Not in plan mode");
            return Ok(());
        }
    };

    // ── Handle pending clarification questions ──────────────────────────
    if let Some(ref mut pending) = plan_state.pending_clarifications
        && let Some(question) = pending.next_question().cloned()
    {
        let answer = parse_clarification_response(&input, &question);
        match answer {
            ClarificationAnswer::Selected(idx) => {
                let selected = &question.options[idx];
                pending.record_answer(selected.clone());
                eprintln!("  {} Selected: {}", theme::icon_ok(), selected);
            }
            ClarificationAnswer::Freeform(text) => {
                pending.record_answer(text.clone());
                eprintln!("  {} Answer: {}", theme::icon_ok(), text);
            }
            ClarificationAnswer::Invalid(msg) => {
                eprintln!("  {} {}", theme::icon_err(), msg);
                eprintln!();
                eprint!("{}", format_clarification_question(&question));
                return Ok(());
            }
        }

        if let Some(next_q) = pending.next_question() {
            eprintln!();
            eprint!("{}", format_clarification_question(next_q));
            let _ = plan_state.save_to_file(&PlanModeState::state_path());
            return Ok(());
        }

        // All questions answered — regenerate plan with clarifications
        eprintln!();
        eprintln!(
            "  {} All clarifications answered. Regenerating plan...",
            "🔄".cyan()
        );

        let answers_context = pending.format_for_prompt();
        let goal_with_context = format!(
            "{}\n\n## Clarifications from user:\n{}",
            plan_state.goal, answers_context
        );

        plan_state.pending_clarifications = None;

        let Some(tok) = token else {
            eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
            return Ok(());
        };

        enrich_with_templates(
            &mut plan_state.context,
            state.matrix_runtime.as_ref(),
            state.ingestion_user_id.as_deref(),
            &goal_with_context,
            state.verbose_mode,
        )
        .await;
        let prompt = decomposition_prompt(&goal_with_context, &plan_state.context);
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": prompt}],
            "session_id": state.session_id.clone(),
        });

        let resp = api.post_chat_turn(tok, &payload).await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let mut full_text = String::new();
                let mut stream = r.bytes_stream();

                eprintln!("  {} Thinking...", "🧠".cyan());

                while let Some(chunk) = stream.next().await {
                    if let Ok(bytes) = chunk {
                        let event_str = String::from_utf8_lossy(&bytes);
                        for line in event_str.lines() {
                            if let Some(data) = line.strip_prefix("data: ")
                                && let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                                && let Some(content) = json.get("content").and_then(|v| v.as_str())
                            {
                                full_text.push_str(content);
                            }
                        }
                    }
                }

                match parse_plan_response(&full_text) {
                    Ok(plan) => {
                        plan_state.set_plan(plan);
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());

                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanEdit,
                            "Plan regenerated after clarifications",
                            None,
                        );

                        eprintln!();
                        eprintln!("{}", format_plan(&plan_state.plan));
                        eprintln!();
                        eprint_plan_commands_help();
                    }
                    Err(e) => {
                        eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    }
                }
            }
            Ok(r) => {
                eprintln!("  {} LLM call failed ({})", theme::icon_err(), r.status());
            }
            Err(e) => {
                eprintln!("  {} Request failed: {}", theme::icon_err(), e);
            }
        }

        return Ok(());
    }

    // ── Try structured PlanCommand first ─────────────────────────────────
    if let Some(cmd) = PlanCommand::parse(&input) {
        return handle_plan_command(cmd, token, state, api).await;
    }

    // ── Handle entry choices (fresh plan mode, no goal set yet) ──────────
    if plan_state.goal.is_empty() {
        let has_plan = !plan_state.plan.subtasks.is_empty();
        let choice = parse_plan_entry_choice(&input, has_plan, state.executing_plan.is_some());

        match choice {
            PlanEntryChoice::Exit => {
                state.plan_mode = None;
                eprintln!();
                eprintln!("  {} Left plan mode → back to normal chat.", "←".cyan());
                return Ok(());
            }
            PlanEntryChoice::Continue => {
                eprintln!("  {} Continuing with current plan", "→".cyan());
                return Ok(());
            }
            PlanEntryChoice::Restart => {
                plan_state.plan = Default::default();
                plan_state.goal = String::new();
                eprintln!(
                    "  {} Plan cleared. Describe what you want to do:",
                    "🔄".yellow()
                );
                return Ok(());
            }
            PlanEntryChoice::Resume => {
                if state.plan_handle.is_some() {
                    eprintln!(
                        "  {} Type {} to resume background execution.",
                        "💡".cyan(),
                        "resume".cyan()
                    );
                } else if state.executing_plan.is_some() {
                    eprintln!("  {} Resuming plan execution...", "▶".cyan());
                }
                return Ok(());
            }
            PlanEntryChoice::New(_) => {
                plan_state.plan = Default::default();
                eprintln!("  {} Describe what you want to do:", "📝".cyan());
                return Ok(());
            }
            PlanEntryChoice::Goal(goal) => {
                return handle_goal_submission(goal, token, state, api).await;
            }
        }
    }

    // ── Check for "done <id>" — manual subtask completion ───────────────
    let input_lower = input.to_lowercase();
    if let Some(done_id) = input_lower.strip_prefix("done ").map(|s| s.trim())
        && !done_id.is_empty()
    {
        if plan_execution_ui_active(state) {
            eprintln!(
                "  {} Cannot use done while a plan run is active (background executor).",
                theme::icon_warn()
            );
            return Ok(());
        }
        let plan_state = state.plan_mode.as_mut().unwrap();
        match plan_state.complete_subtask(done_id) {
            Ok(title) => {
                let pct = plan_state.plan.progress_pct();
                let done_count = plan_state.plan.items_done();
                let total_count = plan_state.plan.subtasks.len();
                eprintln!("  {} Completed: {} ({}%)", theme::icon_ok(), title, pct);
                let _ = plan_state.save_to_file(&PlanModeState::state_path());

                journal_plan_event(
                    &mut state.journal,
                    session_journal::JournalEventType::PlanProgress,
                    &format!("Subtask completed: {title}"),
                    Some(serde_json::json!({
                        "subtask_id": done_id,
                        "progress_pct": pct,
                    })),
                );

                if let Some(ref svc) = state.task_service {
                    use astra_services::TaskService;
                    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                    let goal = &plan_state.goal;
                    if let Ok(tasks) = svc.list_tasks(user_id, None).await
                        && let Some(task) = tasks.iter().find(|t| &t.title == goal)
                    {
                        let _ = svc.update_plan(&task.task_id, &plan_state.plan).await;
                        let _ = svc
                            .update_progress(&task.task_id, pct, done_count, total_count as u32)
                            .await;
                    }
                }

                let ready = plan_state.plan.ready_subtasks();
                if !ready.is_empty() {
                    eprintln!("  {} Next ready:", "→".cyan());
                    for st in &ready {
                        eprintln!("    {} [{}] {}", "○".dim(), st.id, st.title);
                    }
                } else if plan_state.plan.progress_pct() == 100 {
                    eprintln!("  {} All tasks complete!", "🎉".green());
                    if let Some(ref svc) = state.task_service {
                        use astra_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        let goal = &plan_state.goal;
                        if let Ok(tasks) = svc.list_tasks(user_id, None).await
                            && let Some(task) = tasks.iter().find(|t| &t.title == goal)
                        {
                            let _ = svc.complete_task(&task.task_id).await;
                        }
                    }
                    eprintln!();
                    eprintln!(
                        "  {} Rate this plan (1-5)? Or 'skip' to skip: /plan rate <1-5>",
                        "💡".cyan()
                    );
                }
            }
            Err(e) => eprintln!("  {} {}", theme::icon_warn(), e),
        }
        return Ok(());
    }

    // ── Natural-language plan editing via LLM ───────────────────────────
    if plan_execution_ui_active(state) {
        eprintln!(
            "  {} Plan run is active; LLM edits are paused. Try {}, {}, or {}.",
            theme::icon_warn(),
            "status".cyan(),
            "show".cyan(),
            "exit".cyan()
        );
        return Ok(());
    }
    let plan_state = state.plan_mode.as_mut().unwrap();
    let prompt = plan_state.plan_mode_prompt(&input);
    plan_state.add_turn(&input, "");

    eprint!("  ● Thinking...");

    let Some(tok) = token else {
        eprintln!("\r  ✗ Not logged in. Run /login first.");
        return Ok(());
    };

    let messages = vec![serde_json::json!({
        "role": "user",
        "content": prompt
    })];

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let payload = serde_json::json!({
        "messages": messages,
        "model": state.model.clone(),
        "edge_profile": {
            "cwd": cwd.to_string_lossy(),
        },
        "edge_tools": [],
    });

    let resp = api.post_chat_turn(tok, &payload).await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let sse_result = collect_sse_text(r, false).await;

            eprint!("\r                    \r");

            if sse_result.text.is_empty() {
                if sse_result.event_count == 0 {
                    eprintln!(
                        "  {} No SSE events received from server",
                        theme::icon_warn()
                    );
                } else {
                    eprintln!(
                        "  {} {} events (types: {}) but no text",
                        theme::icon_warn(),
                        sse_result.event_count,
                        sse_result.event_types.join(", ")
                    );
                }
            }

            let plan_updated = if !sse_result.text.is_empty() {
                match parse_plan_response(&sse_result.text) {
                    Ok(plan) => {
                        let plan_state = state.plan_mode.as_mut().unwrap();
                        plan_state.set_plan(plan.clone());
                        plan_state.modified = true;
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());

                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanEdit,
                            &format!("Plan edited: {}", input.chars().take(80).collect::<String>()),
                            Some(serde_json::json!({
                                "instruction": input.chars().take(200).collect::<String>(),
                                "subtask_count": plan.subtasks.len(),
                            })),
                        );

                        eprintln!("{}  Plan updated!", theme::icon_ok());
                        eprintln!();
                        let formatted = format_plan(&plan);
                        eprintln!("{formatted}");
                        true
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !sse_result.text.is_empty() && !plan_updated {
                eprintln!();
                eprintln!("{}", sse_result.text.trim());
            }

            if let Some(plan_state) = state.plan_mode.as_mut() {
                if let Some(last) = plan_state.history.last_mut() {
                    last.1 = sse_result.text.chars().take(500).collect();
                }
            }
        }
        Ok(r) => {
            eprintln!("\r  ✗ LLM call failed ({})", r.status());
        }
        Err(e) => {
            eprintln!("\r  ✗ Request failed: {e}");
        }
    }

    Ok(())
}

/// Handle a parsed `PlanCommand`.
async fn handle_plan_command(
    cmd: PlanCommand,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<(), String> {
    use plan::{
        PlanExecutionConfig, PlanModeState, format_execution_preview, format_plan,
    };

    match cmd {
        PlanCommand::Cancel => {
            shutdown_plan_executor(state);

            journal_plan_event(
                &mut state.journal,
                session_journal::JournalEventType::PlanLifecycle,
                "Plan mode cancelled",
                None,
            );

            PlanModeState::clear_saved_state();
            state.plan_mode = None;
            state.executing_plan_goal = None;
            eprintln!();
            eprintln!("  {} Left plan mode → back to normal chat.", "←".cyan());
            eprintln!();
        }

        PlanCommand::Status => {
            if let Some(ref ps) = state.plan_mode {
                if plan_execution_ui_active(state) {
                    let pct = ps.plan.progress_pct();
                    let done = ps.plan.items_done();
                    let total = ps.plan.subtasks.len();
                    let goal_display = state
                        .executing_plan_goal
                        .as_deref()
                        .unwrap_or(ps.goal.as_str());
                    let round = state.plan_execution_rounds;

                    eprintln!("┌── Execution Status ───────────────────────────────");
                    eprintln!("│ Goal:      {goal_display}");
                    eprintln!("│ Phase:     running in background (still in plan mode)");
                    let bar_width = 30;
                    let filled = (pct as usize * bar_width) / 100;
                    let empty = bar_width - filled;
                    let bar = format!(
                        "{}{}",
                        "█".repeat(filled).green(),
                        "░".repeat(empty).dim()
                    );
                    eprintln!("│ Progress:  [{bar}] {done}/{total} ({pct}%) — bar may lag vs live output");
                    eprintln!("│ Round:     {round}");
                    if let Some(ref stid) = state.current_plan_subtask_id {
                        eprintln!("│ Current:   {stid}");
                    }
                    if !state.plan_execution_corrections.is_empty() {
                        eprintln!(
                            "│ Corrections: {} queued",
                            state.plan_execution_corrections.len()
                        );
                    }
                    eprintln!("│");
                    eprintln!("│ Commands: pause | resume | show | help | exit");
                    eprintln!("└───────────────────────────────────────────────────");
                } else {
                    let pct = ps.plan.progress_pct();
                    let done = ps.plan.items_done();
                    let total = ps.plan.subtasks.len();
                    let versions = ps.version_history.versions.len();
                    let edits = ps.history.len();

                    eprintln!("┌── Plan Status ────────────────────────────────────");
                    eprintln!("│ Goal:     {}", ps.goal);
                    let phase = if plan_idle_review_not_started(ps) {
                        format!("review — not started (type {} to run)", "go".cyan())
                    } else {
                        "editing plan".to_string()
                    };
                    eprintln!("│ Phase:    {phase}");
                    eprintln!("│");

                    let bar_width = 30;
                    let filled = (pct as usize * bar_width) / 100;
                    let empty = bar_width - filled;
                    let bar = format!(
                        "{}{}",
                        "█".repeat(filled).green(),
                        "░".repeat(empty).dim()
                    );
                    eprintln!("│ Progress: [{bar}] {done}/{total} ({pct}%)");
                    eprintln!("│ Versions: {versions}  |  Edits: {edits}");

                    let ready = ps.plan.ready_subtasks();
                    if !ready.is_empty() {
                        eprintln!("│");
                        eprintln!("│ {} Ready subtasks:", "→".cyan());
                        for st in &ready {
                            eprintln!("│   {} [{}] {}", "○".dim(), st.id, st.title);
                        }
                    }

                    let blocked: Vec<_> = ps.plan.subtasks.iter()
                        .filter(|s| s.status == astra_services::task_orchestrator::TaskStatus::Pending
                            && !s.depends_on.is_empty()
                            && s.depends_on.iter().any(|dep| {
                                ps.plan.subtasks.iter().any(|d| d.id == *dep && d.status != astra_services::task_orchestrator::TaskStatus::Completed)
                            }))
                        .collect();
                    if !blocked.is_empty() {
                        eprintln!("│");
                        eprintln!("│ {} Blocked subtasks:", "⏳".yellow());
                        for st in &blocked {
                            let deps: Vec<_> = st.depends_on.iter().map(|d| d.as_str()).collect();
                            eprintln!("│   {} [{}] {} (waiting on: {})", "●".dim(), st.id, st.title, deps.join(", "));
                        }
                    }

                    eprintln!("│");
                    eprintln!("│ Commands: execute | step | edit <instruction> | diff | history");
                    eprintln!("└───────────────────────────────────────────────────");
                }
            } else if let Some(plan) = &state.executing_plan {
                let pct = plan.progress_pct();
                let done = plan.items_done();
                let total = plan.subtasks.len();
                let goal = state.executing_plan_goal.as_deref().unwrap_or("(unknown)");
                let round = state.plan_execution_rounds;

                eprintln!("┌── Execution Status ───────────────────────────────");
                eprintln!("│ Goal:      {goal}");
                eprintln!("│ Phase:     executing (round {round})");
                let bar_width = 30;
                let filled = (pct as usize * bar_width) / 100;
                let empty = bar_width - filled;
                let bar = format!(
                    "{}{}",
                    "█".repeat(filled).green(),
                    "░".repeat(empty).dim()
                );
                eprintln!("│ Progress:  [{bar}] {done}/{total} ({pct}%)");

                if let Some(ref stid) = state.current_plan_subtask_id {
                    eprintln!("│ Current:   {stid}");
                }

                let corrections = &state.plan_execution_corrections;
                if !corrections.is_empty() {
                    eprintln!("│ Corrections: {} queued", corrections.len());
                }
                eprintln!("│");
                eprintln!("│ Commands: pause | correct <guidance> | cancel");
                eprintln!("└───────────────────────────────────────────────────");
            } else {
                eprintln!("  No active plan or execution");
            }
        }

        PlanCommand::Execute { step_by_step } => {
            let plan_state = match state.plan_mode.as_ref() {
                Some(ps) => ps,
                None => return Ok(()),
            };

            if plan_execution_ui_active(state) {
                eprintln!(
                    "  {} A plan is already running. Wait for it to finish, or use {} / {}.",
                    theme::icon_warn(),
                    "pause".cyan(),
                    "exit".cyan()
                );
                return Ok(());
            }

            let plan = plan_state.plan.clone();
            let goal = plan_state.goal.clone();

            eprintln!();
            eprint!("{}", format_execution_preview(&plan));
            eprintln!();

            if step_by_step {
                eprintln!(
                    "{}  Step-by-step mode: you'll confirm each subtask before execution.",
                    "⚙".cyan()
                );
                eprintln!(
                    "{}  Staying in plan mode — prompt shows {} while the run is active.",
                    "💡".cyan(),
                    "plan*[…]>".yellow()
                );
                eprintln!();
            }

            // Persist to task service
            if let Some(ref svc) = state.task_service {
                use astra_services::{TaskCreateRequest, TaskService};
                let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                let session_id = state.session_id.as_deref().unwrap_or("no-session");

                let project_type = plan_state
                    .context
                    .languages
                    .first()
                    .map(|s| s.to_lowercase());

                let goal_pattern = Some(extract_goal_pattern(&goal));

                match svc
                    .create_task(
                        user_id,
                        session_id,
                        TaskCreateRequest {
                            title: goal.clone(),
                            description: Some(format!(
                                "Plan Mode: {} subtasks",
                                plan.subtasks.len()
                            )),
                            plan: Some(plan.clone()),
                            parent_task_id: None,
                            project_type,
                            goal_pattern,
                        },
                    )
                    .await
                {
                    Ok(tid) => {
                        let short = &tid[..8.min(tid.len())];
                        eprintln!(
                            "{}  Task created: {} ({})",
                            theme::icon_ok(),
                            goal,
                            short.dim()
                        );
                        eprintln!("{}  Track progress: /task status {}", "💡".cyan(), short);
                    }
                    Err(e) => {
                        eprintln!("{}  Could not persist task: {}", theme::icon_warn(), e);
                    }
                }
            }

            if !step_by_step {
                eprintln!(
                    "{}  Auto-executing plan ({} subtasks)...",
                    "🚀".green(),
                    plan.subtasks.len()
                );
                eprintln!(
                    "{}  Staying in plan mode — prompt becomes {} ({} = running). {} still works.",
                    "💡".cyan(),
                    "plan*[…]>".yellow(),
                    "*".yellow(),
                    "status".cyan()
                );
                eprintln!();
            }

            journal_plan_event(
                &mut state.journal,
                session_journal::JournalEventType::PlanLifecycle,
                &format!(
                    "Plan execution started ({} mode, {} subtasks)",
                    if step_by_step { "step-by-step" } else { "auto" },
                    plan.subtasks.len(),
                ),
                Some(serde_json::json!({
                    "mode": if step_by_step { "step_by_step" } else { "auto" },
                    "subtask_count": plan.subtasks.len(),
                })),
            );

            state.plan_execution_config = Some(PlanExecutionConfig {
                step_by_step,
                auto_execute: !step_by_step,
            });
            state.executing_plan_goal = Some(goal);
            state.plan_execution_rounds = 0;
            state.plan_execution_corrections.clear();
            state.executing_plan = Some(plan);
        }

        PlanCommand::Resume => {
            if let Some(ref handle) = state.plan_handle {
                let corrections = if state.plan_execution_corrections.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut state.plan_execution_corrections))
                };
                match handle.send_command(crate::plan_executor::PlanCommand::Resume { corrections })
                {
                    Ok(()) => {
                        eprintln!("  {} Resuming plan execution...", "▶".cyan());
                        state.plan_resume_pending = true;
                    }
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            } else if state.executing_plan.is_some() {
                eprintln!("  {} Resuming plan execution...", "▶".cyan());
            } else {
                eprintln!("  {} No paused plan to resume", theme::icon_warn());
            }
        }

        PlanCommand::Pause => {
            if let Some(ref handle) = state.plan_handle {
                match handle.send_command(crate::plan_executor::PlanCommand::Pause) {
                    Ok(()) => eprintln!("  {} Pause requested.", "⏸".cyan()),
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            } else {
                eprintln!("  {} Use Ctrl+C during execution to pause", "💡".cyan());
            }
        }

        PlanCommand::Timeline => {
            if let Some(ref ps) = state.plan_mode {
                eprintln!("{}", ps.timeline.format_display());
            } else {
                eprintln!("  (no timeline data)");
            }
        }

        PlanCommand::Metrics => {
            if let Some(ref ps) = state.plan_mode {
                let pct = ps.plan.progress_pct();
                let done = ps.plan.items_done();
                let total = ps.plan.subtasks.len();
                let versions = ps.version_history.versions.len();
                let edits = ps.history.len();
                let timeline_events = ps.timeline.events.len();

                eprintln!("┌── Plan Metrics ───────────────────────────────────");
                eprintln!("│ Progress:  {done}/{total} subtasks ({pct}%)");
                eprintln!("│ Versions:  {versions}");
                eprintln!("│ Edits:     {edits}");
                eprintln!("│ Timeline:  {timeline_events} events");

                if !ps.plan.subtasks.is_empty() {
                    eprintln!("│");
                    eprintln!("│ Subtask breakdown:");
                    for st in &ps.plan.subtasks {
                        let icon = match st.status {
                            astra_services::task_orchestrator::TaskStatus::Completed => "✓".green().to_string(),
                            astra_services::task_orchestrator::TaskStatus::Failed => "✗".red().to_string(),
                            astra_services::task_orchestrator::TaskStatus::InProgress => "▶".cyan().to_string(),
                            _ => "○".dim().to_string(),
                        };
                        let deps = if st.depends_on.is_empty() {
                            String::new()
                        } else {
                            format!(" (deps: {})", st.depends_on.join(", "))
                        };
                        eprintln!("│   {icon} [{}] {}{deps}", st.id, st.title);
                    }
                }
                eprintln!("└───────────────────────────────────────────────────");
            } else {
                eprintln!("  No active plan for metrics");
            }
        }

        PlanCommand::History => {
            if let Some(ref ps) = state.plan_mode {
                eprintln!("{}", ps.version_history.format_log());
            } else {
                eprintln!("  No version history");
            }
        }

        PlanCommand::Show => {
            if let Some(ref ps) = state.plan_mode {
                eprintln!();
                eprintln!("{}", format_plan(&ps.plan));
            } else {
                eprintln!("  No active plan");
            }
        }

        PlanCommand::Diff { from, to } => {
            if let Some(ref ps) = state.plan_mode {
                let f = from.unwrap_or(ps.version_history.current_version.saturating_sub(1));
                let t = to.unwrap_or(ps.version_history.current_version);
                match ps.version_history.diff_versions(f, t) {
                    Ok(diff) => eprintln!("{}", diff.format()),
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            }
        }

        PlanCommand::Rollback { version } => {
            if plan_execution_ui_active(state) {
                eprintln!(
                    "  {} Rollback is disabled while a plan run is active. Pause or cancel first.",
                    theme::icon_warn()
                );
                return Ok(());
            }
            if let Some(ref mut ps) = state.plan_mode {
                match ps.rollback_to_version(version) {
                    Ok(msg) => {
                        let _ = ps.save_to_file(&PlanModeState::state_path());
                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanEdit,
                            &format!("Plan rolled back to v{version}"),
                            None,
                        );
                        eprintln!("  {} {}", theme::icon_ok(), msg);
                        eprintln!();
                        eprintln!("{}", format_plan(&ps.plan));
                    }
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            }
        }

        PlanCommand::List => {
            let plans = plan::list_saved_plans();
            eprintln!("{}", plan::format_plan_list(&plans));
        }

        PlanCommand::Correct { guidance } => {
            state.plan_execution_corrections.push(guidance.clone());
            eprintln!(
                "  {} Correction queued ({} total)",
                theme::icon_ok(),
                state.plan_execution_corrections.len()
            );
        }

        PlanCommand::ClearCorrections => {
            let cleared = state.plan_execution_corrections.len();
            state.plan_execution_corrections.clear();
            eprintln!("  {} Cleared {} correction(s)", theme::icon_ok(), cleared);
        }

        PlanCommand::Rewind { anchor } => {
            if let Some(ref mut plan) = state.executing_plan {
                let anchor_parsed = if let Ok(n) = anchor.parse::<usize>() {
                    plan::PlanRewindAnchor::OneBased(n)
                } else {
                    plan::PlanRewindAnchor::IdPrefix(anchor.clone())
                };
                match plan::resolve_rewind_start_index(plan, &anchor_parsed) {
                    Ok(idx) => {
                        let count = plan::rewind_plan_from_subtask(plan, idx);
                        eprintln!(
                            "  {} Rewound {} subtask(s) from '{}'",
                            theme::icon_ok(),
                            count,
                            anchor,
                        );
                    }
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            } else if plan_execution_ui_active(state) {
                eprintln!(
                    "  {} Rewind targets the live executor after a pause. Pause the run first ({} or Ctrl+C).",
                    theme::icon_warn(),
                    "pause".cyan()
                );
            } else if state.plan_mode.is_some() {
                eprintln!("  {} Rewind is only available during execution", theme::icon_warn());
            }
        }

        PlanCommand::EnablePlanOnly => {
            state.chat_plan_only = true;
            eprintln!("  {} Plan-only chat enabled (tools disabled)", theme::icon_ok());
        }

        PlanCommand::DisablePlanOnly => {
            state.chat_plan_only = false;
            eprintln!("  {} Plan-only chat disabled (tools re-enabled)", theme::icon_ok());
        }

        PlanCommand::Approve { .. } | PlanCommand::Reject { .. } => {
            eprintln!("  {} Approval commands are handled by the execution loop", "💡".cyan());
        }

        PlanCommand::Help => {
            let goal = state.plan_mode.as_ref().map(|ps| ps.goal.as_str()).unwrap_or("");
            eprint_plan_mode_banner(goal);
        }

        PlanCommand::Create { goal } => {
            return handle_goal_submission(goal, token, state, api).await;
        }

        PlanCommand::Edit { instruction } => {
            if plan_execution_ui_active(state) {
                eprintln!(
                    "  {} Cannot edit the plan via LLM while a plan run is active.",
                    theme::icon_warn()
                );
                return Ok(());
            }
            return Box::pin(handle_plan_mode_input(instruction, token, state, api)).await;
        }
    }

    Ok(())
}

/// Handle initial goal submission — scan project and generate plan via LLM.
async fn handle_goal_submission(
    goal: String,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<(), String> {
    use plan::{
        PendingClarifications, PlanModeState, decomposition_prompt,
        detect_clarification_questions, format_clarification_question, format_plan,
        format_project_context, parse_plan_response,
    };

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
        return Ok(());
    };

    let plan_state = state.plan_mode.as_mut().unwrap();
    plan_state.goal = goal.clone();

    journal_plan_event(
        &mut state.journal,
        session_journal::JournalEventType::PlanLifecycle,
        &format!("Plan mode started: {}", goal.chars().take(80).collect::<String>()),
        Some(serde_json::json!({
            "goal": goal,
        })),
    );

    eprintln!();
    eprintln!("{}", format_project_context(&plan_state.context));
    eprintln!();

    eprintln!("  {} Thinking...", "🧠".cyan());
    eprintln!();

    enrich_with_templates(
        &mut plan_state.context,
        state.matrix_runtime.as_ref(),
        state.ingestion_user_id.as_deref(),
        &goal,
        state.verbose_mode,
    )
    .await;
    let prompt = decomposition_prompt(&goal, &plan_state.context);
    let payload = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
        "session_id": state.session_id.clone(),
    });

    let resp = api.post_chat_turn(tok, &payload).await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let mut full_text = String::new();
            let mut stream = r.bytes_stream();

            let mut in_thinking = false;
            let mut in_plan_json = false;
            let mut chars_since_nl = 0;

            while let Some(chunk) = stream.next().await {
                if let Ok(bytes) = chunk {
                    let event_str = String::from_utf8_lossy(&bytes);
                    for line in event_str.lines() {
                        if let Some(data) = line.strip_prefix("data: ")
                            && let Ok(json) =
                                serde_json::from_str::<serde_json::Value>(data)
                            && json.get("type").and_then(|v| v.as_str())
                                == Some("text_delta")
                            && let Some(content) =
                                json.get("content").and_then(|v| v.as_str())
                        {
                            full_text.push_str(content);

                            for ch in content.chars() {
                                if ch == '{' && !in_thinking && !in_plan_json {
                                    in_plan_json = true;
                                    eprintln!();
                                    eprintln!();
                                    eprint!("  {} Parsing plan", "⚙".dim());
                                    continue;
                                }

                                if in_plan_json {
                                    if ch == ',' || ch == '}' {
                                        eprint!(".");
                                    }
                                    continue;
                                }

                                if !in_thinking && chars_since_nl == 0 {
                                    in_thinking = true;
                                    eprint!("  ");
                                }

                                eprint!("{}", ch);

                                if ch == '\n' {
                                    chars_since_nl = 0;
                                    in_thinking = false;
                                } else {
                                    chars_since_nl += 1;
                                }
                            }
                        }
                    }
                }
            }
            eprintln!();

            if let Some(questions) = detect_clarification_questions(&full_text) {
                eprintln!();
                eprintln!(
                    "  {} Need clarification before generating plan:",
                    "❓".yellow()
                );
                eprintln!();

                let plan_state = state.plan_mode.as_mut().unwrap();
                let pending = PendingClarifications {
                    questions: questions.clone(),
                    answers: Vec::new(),
                };
                plan_state.pending_clarifications = Some(pending);

                eprint!("{}", format_clarification_question(&questions[0]));
                let _ = plan_state.save_to_file(&PlanModeState::state_path());
            } else {
                match parse_plan_response(&full_text) {
                    Ok(plan) => {
                        let plan_state = state.plan_mode.as_mut().unwrap();
                        plan_state.set_plan(plan);
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());

                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanLifecycle,
                            &format!("Plan generated: {} subtasks", plan_state.plan.subtasks.len()),
                            Some(serde_json::json!({
                                "subtask_count": plan_state.plan.subtasks.len(),
                            })),
                        );

                        eprintln!();
                        eprintln!("{}", format_plan(&plan_state.plan));
                        eprintln!();
                        eprint_plan_commands_help();
                    }
                    Err(e) => {
                        eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    }
                }
            }
        }
        Ok(r) => {
            eprintln!("  {} LLM call failed ({})", theme::icon_err(), r.status());
        }
        Err(e) => {
            eprintln!("  {} Request failed: {}", theme::icon_err(), e);
        }
    }

    Ok(())
}

/// Extract a normalized goal pattern for matching similar tasks.
pub(super) fn extract_goal_pattern(goal: &str) -> String {
    let task_verbs = [
        "add",
        "fix",
        "implement",
        "create",
        "update",
        "refactor",
        "remove",
        "delete",
        "optimize",
        "improve",
        "migrate",
        "integrate",
        "test",
        "document",
        "configure",
    ];

    let goal_lower = goal.to_lowercase();
    let words: Vec<&str> = goal_lower.split_whitespace().collect();
    if words.is_empty() {
        return "*".to_string();
    }

    let mut pattern_parts = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let word = words[i];

        if task_verbs.contains(&word)
            || ["for", "to", "in", "with", "from", "by", "the", "a", "an"].contains(&word)
            || [
                "api", "endpoint", "database", "file", "module", "function", "class", "test",
                "config", "error", "logging", "auth", "user", "data", "cache", "queue",
            ]
            .contains(&word)
        {
            pattern_parts.push(word.to_string());
        } else if word.contains('.') || word.contains('/') || word.contains('_') {
            pattern_parts.push("*".to_string());
        } else if word.len() <= 4 {
            pattern_parts.push(word.to_string());
        } else {
            pattern_parts.push("*".to_string());
        }

        i += 1;
    }

    let mut result = Vec::new();
    for part in pattern_parts {
        if part == "*" && result.last() == Some(&"*".to_string()) {
            continue;
        }
        result.push(part);
    }

    result.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_executor;
    use astra_services::task_orchestrator::{SubtaskPlan, TaskStatus};

    #[test]
    fn plan_execution_ui_active_follows_handle() {
        let state = ReplState::default();
        assert!(
            !plan_execution_ui_active(&state),
            "no handle => inactive"
        );

        let mut state = ReplState::default();
        let (handle, _update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        assert!(
            plan_execution_ui_active(&state),
            "handle present => active"
        );
    }

    #[test]
    fn plan_idle_review_not_started_empty_subtasks() {
        let ctx = plan::ProjectContext::default();
        let ps = plan::PlanModeState::new("goal".into(), ctx);
        assert!(!plan_idle_review_not_started(&ps));
    }

    #[test]
    fn plan_idle_review_not_started_all_pending() {
        let ctx = plan::ProjectContext::default();
        let mut ps = plan::PlanModeState::new("goal".into(), ctx);
        ps.plan.subtasks.push(SubtaskPlan {
            id: "s1".into(),
            title: "one".into(),
            ..Default::default()
        });
        assert!(plan_idle_review_not_started(&ps));
    }

    #[test]
    fn plan_idle_review_not_started_any_non_pending() {
        let ctx = plan::ProjectContext::default();
        let mut ps = plan::PlanModeState::new("goal".into(), ctx);
        ps.plan.subtasks.push(SubtaskPlan {
            id: "s1".into(),
            title: "done".into(),
            status: TaskStatus::Completed,
            ..Default::default()
        });
        ps.plan.subtasks.push(SubtaskPlan {
            id: "s2".into(),
            title: "wait".into(),
            ..Default::default()
        });
        assert!(!plan_idle_review_not_started(&ps));
    }

    #[test]
    fn extract_goal_pattern_empty_string_returns_wildcard() {
        assert_eq!(extract_goal_pattern(""), "*");
    }

    #[test]
    fn extract_goal_pattern_whitespace_only_returns_wildcard() {
        assert_eq!(extract_goal_pattern("   "), "*");
    }

    #[test]
    fn extract_goal_pattern_preserves_task_verbs() {
        assert!(extract_goal_pattern("add feature").starts_with("add"));
        assert!(extract_goal_pattern("fix bug").starts_with("fix"));
        assert!(extract_goal_pattern("implement api").starts_with("implement"));
    }

    #[test]
    fn extract_goal_pattern_replaces_long_unknown_words() {
        let p = extract_goal_pattern("add authentication feature");
        assert!(p.contains("*"));
    }

    #[test]
    fn shutdown_plan_executor_returns_false_when_no_handle() {
        let mut state = ReplState::default();
        assert!(!shutdown_plan_executor(&mut state));
    }

    #[test]
    fn shutdown_plan_executor_cancels_and_drains_handle() {
        let mut state = ReplState::default();
        let (handle, update_tx, mut cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);

        // Send some trailing updates before shutdown
        let _ = update_tx.send(plan_executor::PlanUpdate::PlanCompleted {
            pct: 100,
            elapsed: std::time::Duration::from_secs(10),
        });

        let had_handle = shutdown_plan_executor(&mut state);
        assert!(had_handle, "should return true when handle was present");
        assert!(state.plan_handle.is_none(), "handle should be cleared");

        // The Cancel command should have been sent
        let cmd = cmd_rx.try_recv();
        assert!(
            matches!(cmd, Ok(plan_executor::PlanCommand::Cancel)),
            "expected Cancel command, got {:?}",
            cmd
        );
    }
}
