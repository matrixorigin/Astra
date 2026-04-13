//! Plan mode interaction handler — extracted from `slash_memory.rs`.
//!
//! Handles all user input while in the interactive plan editing mode (`plan>` prompt).
//! Uses the `PlanCommand` parser for structured commands and falls back to
//! natural-language plan editing via LLM.

use super::*;
use crate::sse_utils::collect_sse_with_preview;
use astra_runtime::plan;
use astra_runtime::plan::PlanCommand;
use astra_runtime::plan::progress_bar_segments;
use astra_services::session_journal;
use crossterm::style::Stylize;

/// Replace `plan_state.plan` when `text` is non-empty valid plan JSON.
///
/// Returns `Ok(false)` if `text` is empty or whitespace only; `Ok(true)` if the plan
/// was replaced; `Err` if the text is non-empty but not valid plan JSON.
pub(crate) fn try_replace_plan_from_llm_json(
    text: &str,
    plan_state: &mut plan::PlanModeState,
) -> Result<bool, String> {
    if text.trim().is_empty() {
        return Ok(false);
    }
    // If the response contains no JSON-like content, treat it as a
    // natural-language answer (not an error).
    if !text.contains('{') {
        return Ok(false);
    }
    match plan::parse_plan_response(text) {
        Ok(mut new_plan) => {
            // Preserve completed subtasks that LLM may have dropped
            let old_completed: Vec<_> = plan_state
                .plan
                .subtasks
                .iter()
                .filter(|s| s.status == astra_services::task_orchestrator::TaskStatus::Completed)
                .collect();
            if !old_completed.is_empty() {
                // Collect subtasks to prepend (in original order) and track existing ones
                let mut to_prepend = Vec::new();
                for old in &old_completed {
                    let kept = new_plan.subtasks.iter().any(|n| n.id == old.id);
                    if !kept {
                        to_prepend.push((*old).clone());
                    } else {
                        // Ensure status stays completed even if LLM reset it
                        if let Some(n) = new_plan.subtasks.iter_mut().find(|n| n.id == old.id) {
                            n.status = astra_services::task_orchestrator::TaskStatus::Completed;
                        }
                    }
                }
                // Prepend missing completed subtasks in their original order
                if !to_prepend.is_empty() {
                    to_prepend.append(&mut new_plan.subtasks);
                    new_plan.subtasks = to_prepend;
                }
            }
            plan_state.set_plan(new_plan);
            Ok(true)
        }
        Err(e) => Err(e),
    }
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
                "  {} {}",
                "⋯".dim(),
                "No cloud connection — skipping template search".dim()
            );
        }
        return;
    };
    let pool = mc.shared_pool().get();
    let uid = user_id.unwrap_or("anonymous");

    if verbose {
        eprintln!(
            "  {} {}",
            "⋯".cyan(),
            "Searching for similar plan templates…".dim()
        );
    }

    let templates = plan::query_similar_templates(pool, uid, goal, 3).await;
    if !templates.is_empty() {
        eprintln!(
            "  {} Found {} learned template{}",
            theme::icon_ok(),
            format!("{}", templates.len()).cyan(),
            if templates.len() == 1 { "" } else { "s" }
        );
        context.prior_templates = templates;
    } else if verbose {
        eprintln!("  {} {}", "⋯".dim(), "No matching templates found".dim());
    }
}

pub(super) fn eprint_plan_json_parse_failed(full_text: &str, _err: &str) {
    eprintln!(
        "  {} Plan response was not structured JSON — showing as text:",
        theme::icon_warn()
    );
    eprintln!();

    // Render the model's reply as markdown so the user at least gets useful output
    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let mut md = super::streaming_md::StreamingMarkdown::new(tw);
    md.push(full_text);
    md.finish();
    eprintln!();

    eprintln!(
        "  {}",
        "Tip: try rephrasing, or type `exit` to leave plan mode and use normal chat.".dim()
    );
}

/// Print available plan mode commands (compact, for after plan generation).
pub(super) fn eprint_plan_commands_help() {
    eprintln!(
        "  {} {} to run  {} {} to modify  {} {} to leave",
        "▸".cyan(),
        "go".bold().cyan(),
        "▸".dim(),
        "describe changes".dim(),
        "▸".dim(),
        "exit".cyan(),
    );
}

/// Branded `inquire` theme: cyan prompt, bold+cyan highlight, dim unselected.
fn plan_select_theme() -> inquire::ui::RenderConfig<'static> {
    use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet};
    let cyan = Color::Rgb {
        r: 0,
        g: 200,
        b: 200,
    };
    let mut rc = RenderConfig::default_colored();
    rc.prompt_prefix = inquire::ui::Styled::new("▸").with_fg(cyan);
    rc.highlighted_option_prefix = inquire::ui::Styled::new("▸").with_fg(cyan);
    rc.selected_option = Some(StyleSheet::new().with_fg(cyan).with_attr(Attributes::BOLD));
    rc.answer = StyleSheet::new().with_fg(cyan).with_attr(Attributes::BOLD);
    rc
}

/// Display a clarification question with modern styled formatting.
///
/// Category icons are kept, options use `▸` prefix for default, dim `·` for others.
pub(super) fn eprint_clarification_question(
    q: &astra_runtime::plan_decompose::ClarificationQuestion,
) {
    let icon = match q.category {
        astra_runtime::plan_decompose::ClarificationCategory::Scope => "📦",
        astra_runtime::plan_decompose::ClarificationCategory::Approach => "🛤️ ",
        astra_runtime::plan_decompose::ClarificationCategory::Behavior => "⚙️ ",
        astra_runtime::plan_decompose::ClarificationCategory::Technical => "🔧",
        astra_runtime::plan_decompose::ClarificationCategory::Confirmation => "❓",
    };

    eprintln!("  {} {}", icon, q.question.as_str().bold().cyan());
    eprintln!();

    for (i, opt) in q.options.iter().enumerate() {
        let num = i + 1;
        let is_default = q.default == Some(i);
        if is_default {
            eprintln!(
                "  {} {} {}",
                "▸".cyan(),
                format!("[{num}]").cyan(),
                format!("{opt} (default)").bold()
            );
        } else {
            eprintln!("    {} {}", format!("[{num}]").dim(), opt.as_str().dim());
        }
    }

    eprintln!();
    eprint!("  {} ", "→".cyan());
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

/// Result of the interactive plan confirmation prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanConfirmChoice {
    ExecuteAll,
    StepByStep,
    Edit,
    Cancel,
}

/// Show an interactive selection prompt after plan generation.
///
/// Falls back to `eprint_plan_commands_help()` when stdin is not a terminal
/// or `inquire` returns an error (e.g. user presses Esc).
pub(super) fn prompt_plan_confirmation(subtask_count: usize) -> Option<PlanConfirmChoice> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        eprint_plan_commands_help();
        return None;
    }

    let options = vec![
        format!("▶  Execute all ({subtask_count} subtasks)"),
        "⚙  Step-by-step (confirm each subtask)".to_string(),
        "✏  Edit plan (describe changes)".to_string(),
        "✕  Cancel (back to prompt)".to_string(),
    ];

    eprintln!(); // spacing before prompt
    match inquire::Select::new("Plan ready —", options)
        .with_render_config(plan_select_theme())
        .without_help_message()
        .prompt()
    {
        Ok(choice) if choice.starts_with('▶') => Some(PlanConfirmChoice::ExecuteAll),
        Ok(choice) if choice.starts_with('⚙') => Some(PlanConfirmChoice::StepByStep),
        Ok(choice) if choice.starts_with('✏') => Some(PlanConfirmChoice::Edit),
        Ok(_) => Some(PlanConfirmChoice::Cancel),
        Err(_) => {
            // User pressed Esc or other interrupt
            Some(PlanConfirmChoice::Cancel)
        }
    }
}

/// Print a styled execution preview — replaces plain `format_execution_preview()`.
fn eprint_styled_execution_preview(plan: &astra_runtime::plan::TaskPlan) {
    let analysis = plan::analyze_parallelism(plan);
    let ready = plan.ready_subtasks();

    eprintln!(
        "  {} {} subtasks, {} ready",
        "Execution:".bold(),
        format!("{}", plan.subtasks.len()).cyan(),
        format!("{}", ready.len()).green()
    );

    if analysis.groups.len() > 1 || analysis.groups.first().map(|g| g.len()).unwrap_or(0) > 1 {
        for (i, group) in analysis.groups.iter().enumerate() {
            let ids: Vec<_> = group
                .iter()
                .map(|id| format!("{}", id.as_str().cyan()))
                .collect();
            let parallel = if group.len() > 1 {
                format!(" {}", "(parallel)".dim())
            } else {
                String::new()
            };
            eprintln!(
                "    {} {}{}: {}",
                "›".dim(),
                format!("Round {}", i + 1).dim(),
                parallel,
                ids.join(", ")
            );
        }
    }

    if !analysis.conflicts.is_empty() {
        eprint!(
            "    {} {} file conflict(s): ",
            theme::icon_warn(),
            analysis.conflicts.len()
        );
        let strs: Vec<_> = analysis
            .conflicts
            .iter()
            .map(|c| {
                format!(
                    "{} ↔ {} ({})",
                    c.subtask_a,
                    c.subtask_b,
                    c.shared_files.join(", ")
                )
            })
            .collect();
        eprintln!("{}", strs.join(", ").yellow());
    }

    let total_effort: usize = plan
        .subtasks
        .iter()
        .map(|s| match s.effort.as_deref() {
            Some("large") => 3,
            Some("medium") => 2,
            _ => 1,
        })
        .sum();
    let effort_label = match total_effort {
        0..=3 => "low".green().to_string(),
        4..=8 => "medium".yellow().to_string(),
        _ => "high".red().to_string(),
    };
    eprintln!(
        "  {} {} ({} units)",
        "Effort:".bold(),
        effort_label,
        format!("{total_effort}").dim()
    );
}

/// Compact colored progress bar — uses `astra_runtime::plan::progress_bar_segments` so `filled`
/// is clamped to `width` (avoids panic on edge `pct` values).
fn format_progress_bar(pct: u32, width: usize) -> String {
    let (filled, empty) = progress_bar_segments(pct, width);
    format!("{}{}", "█".repeat(filled).green(), "░".repeat(empty).dim())
}

/// Print the full plan mode banner (shown on entry and on `help` command).
pub(super) fn eprint_plan_mode_banner(goal: &str) {
    eprintln!();
    eprint!("{}", "Plan mode".yellow().bold());
    if !goal.is_empty() {
        let display_goal: String = goal.chars().take(60).collect();
        let suffix = if goal.len() > 60 { "…" } else { "" };
        eprint!(" — {}{}", display_goal.cyan(), suffix);
    }
    eprintln!();
    eprintln!(
        "{}",
        "  go  execute · step  step-by-step · pause · resume · exit · show · status".dim()
    );
    eprintln!(
        "{}",
        "  correct <…> · rewind <…> · diff · rollback · timeline · metrics · history · list".dim()
    );
    eprintln!();
}

/// Render plan markdown progressively through `StreamingMarkdown`.
///
/// Instead of dumping the entire plan at once via `eprintln!`, this function
/// pushes each subtask block independently through the incremental markdown
/// renderer. Each subtask is a self-contained block separated by `\n\n`,
/// which aligns perfectly with `StreamingMarkdown`'s block boundary detection.
/// This produces a smooth, progressive rendering effect in the terminal.
fn eprint_plan_markdown_streaming(plan: &astra_runtime::plan::TaskPlan, goal: Option<&str>) {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        // Non-terminal: fall back to all-at-once
        eprintln!("{}", astra_runtime::plan::format_plan_markdown(plan, goal));
        return;
    }
    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let mut md = streaming_md::StreamingMarkdown::new(tw);

    // Header block
    if let Some(g) = goal {
        md.push(&format!("**Plan:** {g}\n\n"));
    }
    if let Some(ref notes) = plan.notes {
        md.push(&format!("{notes}\n\n"));
    }

    // Each subtask as an independent block
    for (i, st) in plan.subtasks.iter().enumerate() {
        let block = format_subtask_block(i, st);
        md.push(&block);
    }

    // Footer
    md.push("---\n");
    let summary = format_plan_summary_line(plan);
    md.push(&format!("{summary}\n"));

    md.finish();
    // Newline after the rendered output (StreamingMarkdown uses stdout)
    println!();
}

/// Format a single subtask as a markdown block (ending with `\n\n`).
fn format_subtask_block(index: usize, st: &astra_runtime::plan::SubtaskPlan) -> String {
    use astra_runtime::plan::TaskStatus;

    let status_icon = match st.status {
        TaskStatus::Completed => "✓",
        TaskStatus::InProgress => "▶",
        TaskStatus::Failed => "✗",
        TaskStatus::Paused => "⏸",
        _ => "○",
    };

    let effort_badge = match st.effort.as_deref() {
        Some("small") => " `S`",
        Some("medium") => " `M`",
        Some("large") => " `L`",
        _ => "",
    };

    let mut block = format!(
        "{}. {} **{}**{} — {}\n",
        index + 1,
        status_icon,
        st.id,
        effort_badge,
        st.title,
    );

    if let Some(ref desc) = st.description {
        block.push_str(&format!("   {desc}\n"));
    }

    if !st.files.is_empty() {
        let files: Vec<_> = st.files.iter().map(|f| format!("`{f}`")).collect();
        block.push_str(&format!("   Files: {}\n", files.join(", ")));
    }

    if !st.acceptance_checks.is_empty() {
        let checks: Vec<_> = st
            .acceptance_checks
            .iter()
            .map(|vk| {
                use astra_services::durable_task::VerifierKind;
                match vk {
                    VerifierKind::FileExists { paths } => {
                        format!("`file_exists: {}`", paths.join(", "))
                    }
                    VerifierKind::ReadFileContains { path, .. } => format!("`read_file: {path}`"),
                    VerifierKind::GrepCheck { file, pattern, .. } => {
                        format!("`grep '{pattern}' {file}`")
                    }
                    VerifierKind::Command { cmd, .. } => format!("`{cmd}`"),
                    VerifierKind::BuildPass { cmd } => format!("`build: {cmd}`"),
                    VerifierKind::TestPass { cmd, .. } => format!("`test: {cmd}`"),
                    _ => "`check`".into(),
                }
            })
            .collect();
        block.push_str(&format!("   Verify: {}\n", checks.join(", ")));
    }

    if !st.depends_on.is_empty() {
        block.push_str(&format!(
            "   _(depends on: {})_\n",
            st.depends_on.join(", ")
        ));
    }

    block.push('\n');
    block
}

/// Build the summary line (effort counts + progress) for plan footer.
fn format_plan_summary_line(plan: &astra_runtime::plan::TaskPlan) -> String {
    let mut parts = Vec::new();
    let small = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("small"))
        .count();
    let medium = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("medium"))
        .count();
    let large = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("large"))
        .count();
    if small + medium + large > 0 {
        let mut effort = Vec::new();
        if small > 0 {
            effort.push(format!("{small} small"));
        }
        if medium > 0 {
            effort.push(format!("{medium} medium"));
        }
        if large > 0 {
            effort.push(format!("{large} large"));
        }
        parts.push(effort.join(", "));
    }
    parts.push(format!(
        "{}% ({}/{})",
        plan.progress_pct(),
        plan.items_done(),
        plan.subtasks.len(),
    ));
    parts.join(" | ")
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
        _ => session_journal::JournalEvent::plan_lifecycle(None, summary, metadata),
    };
    let _ = writer.append(&event);
}

pub(super) fn journal_goal_steering_event(
    journal: &mut Option<session_journal::JournalWriter>,
    turn: u32,
    source: &str,
    previous_goal: Option<&str>,
    new_goal: &str,
    metadata: Option<serde_json::Value>,
) {
    let Some(writer) = journal else {
        return;
    };
    let _ = writer.append(&session_journal::JournalEvent::goal_steered(
        None,
        turn,
        source,
        previous_goal,
        new_goal,
        metadata,
    ));
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
        ClarificationAnswer, PlanEntryChoice, PlanModeState, decomposition_prompt,
        parse_clarification_response, parse_plan_entry_choice, parse_plan_response,
    };

    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  {} {}", theme::icon_warn(), "Not in plan mode".yellow());
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
                eprintln!(
                    "  {} Selected: {}",
                    theme::icon_ok(),
                    selected.as_str().cyan()
                );
            }
            ClarificationAnswer::Freeform(text) => {
                pending.record_answer(text.clone());
                eprintln!("  {} Answer: {}", theme::icon_ok(), text.as_str().cyan());
            }
            ClarificationAnswer::Invalid(msg) => {
                eprintln!("  {} {}", theme::icon_err(), msg);
                eprintln!();
                eprint_clarification_question(&question);
                return Ok(());
            }
        }

        if let Some(next_q) = pending.next_question() {
            eprintln!();
            eprint_clarification_question(next_q);
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
            eprintln!(
                "  {} {}",
                theme::icon_err(),
                "Not logged in. Run /login first.".red()
            );
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

        eprintln!();
        let resp = api.post_chat_turn(tok, &payload).await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let sse_result = collect_sse_with_preview(r).await;
                if let Some(err) = sse_result.completion_error() {
                    eprintln!("  {} {}", theme::icon_err(), err.red());
                    return Ok(());
                }
                let full_text = sse_result.text;

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

                        let subtask_count = plan_state.plan.subtasks.len();
                        eprint_plan_commands_help();

                        // Offer interactive confirmation if terminal supports it
                        if let Some(choice) = prompt_plan_confirmation(subtask_count) {
                            match choice {
                                PlanConfirmChoice::ExecuteAll => {
                                    return Box::pin(handle_plan_mode_input(
                                        "go".into(),
                                        token,
                                        state,
                                        api,
                                    ))
                                    .await;
                                }
                                PlanConfirmChoice::StepByStep => {
                                    return Box::pin(handle_plan_mode_input(
                                        "step".into(),
                                        token,
                                        state,
                                        api,
                                    ))
                                    .await;
                                }
                                PlanConfirmChoice::Edit | PlanConfirmChoice::Cancel => {}
                            }
                        }
                    }
                    Err(e) => {
                        eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    }
                }
            }
            Ok(r) => {
                eprintln!(
                    "  {} LLM call failed ({})",
                    theme::icon_err(),
                    r.status().to_string().red()
                );
            }
            Err(e) => {
                cli_utils::eprint_request_error(&e);
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
        let Some(plan_state) = state.plan_mode.as_mut() else {
            return Ok(());
        };
        match plan_state.complete_subtask(done_id) {
            Ok(title) => {
                let pct = plan_state.plan.progress_pct();
                let done_count = plan_state.plan.items_done();
                let total_count = plan_state.plan.subtasks.len();
                eprintln!(
                    "  {} Completed: {} ({}%)",
                    theme::icon_ok(),
                    title.as_str().cyan(),
                    pct
                );
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
                    eprintln!("  {} All tasks complete!", "✓".green());
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
    let Some(plan_state) = state.plan_mode.as_mut() else {
        return Ok(());
    };
    let prompt = plan_state.plan_mode_prompt(&input);
    plan_state.add_turn(&input, "");

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
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

    eprintln!();
    let resp = api.post_chat_turn(tok, &payload).await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let sse_result = collect_sse_with_preview(r).await;
            if let Some(err) = sse_result.completion_error() {
                eprintln!("  {} {}", theme::icon_err(), err.red());
                return Ok(());
            }

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

            if !sse_result.text.is_empty() {
                let Some(plan_state) = state.plan_mode.as_mut() else {
                    return Ok(());
                };
                match try_replace_plan_from_llm_json(&sse_result.text, plan_state) {
                    Ok(true) => {
                        plan_state.modified = true;
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());

                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanEdit,
                            &format!(
                                "Plan edited: {}",
                                input.chars().take(80).collect::<String>()
                            ),
                            Some(serde_json::json!({
                                "instruction": input.chars().take(200).collect::<String>(),
                                "subtask_count": plan_state.plan.subtasks.len(),
                            })),
                        );

                        // Auto-prompt execution if there are new pending subtasks
                        let pending_count = plan_state
                            .plan
                            .subtasks
                            .iter()
                            .filter(|s| {
                                s.status == astra_services::task_orchestrator::TaskStatus::Pending
                            })
                            .count();
                        if pending_count > 0 {
                            eprintln!();
                            eprintln!(
                                "  {} {} new subtask{} added.",
                                theme::icon_ok(),
                                format!("{pending_count}").cyan(),
                                if pending_count == 1 { "" } else { "s" }
                            );
                            if let Some(choice) = prompt_plan_confirmation(pending_count) {
                                match choice {
                                    PlanConfirmChoice::ExecuteAll => {
                                        return Box::pin(handle_plan_mode_input(
                                            "go".into(),
                                            token,
                                            state,
                                            api,
                                        ))
                                        .await;
                                    }
                                    PlanConfirmChoice::StepByStep => {
                                        return Box::pin(handle_plan_mode_input(
                                            "step".into(),
                                            token,
                                            state,
                                            api,
                                        ))
                                        .await;
                                    }
                                    PlanConfirmChoice::Edit | PlanConfirmChoice::Cancel => {}
                                }
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!(
                            "  {} Model reply is not valid plan JSON: {}",
                            theme::icon_warn(),
                            e
                        );
                        eprintln!(
                            "  {} Plan unchanged. Try {} for the current plan.",
                            "⋯".dim(),
                            "show".cyan()
                        );
                    }
                }
            }

            if let Some(plan_state) = state.plan_mode.as_mut() {
                if let Some(last) = plan_state.history.last_mut() {
                    last.1 = sse_result.text.chars().take(500).collect();
                }
            }
        }
        Ok(r) => {
            cli_utils::eprint_api_error(r.status().as_u16(), "LLM call failed");
        }
        Err(e) => {
            cli_utils::eprint_request_error(&e);
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
    use plan::{PlanExecutionConfig, PlanModeState};

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

                    eprintln!("  {} {}", "Plan:".bold().cyan(), goal_display);
                    let bar = format_progress_bar(pct, 20);
                    eprintln!(
                        "  {} {bar} {}/{} {}  {}",
                        "Progress:".dim(),
                        format!("{done}").cyan(),
                        format!("{total}").dim(),
                        format!("({pct}%)").bold(),
                        format!("round {round}").dim(),
                    );
                    if let Some(ref stid) = state.current_plan_subtask_id {
                        eprintln!("  {} {}", "Current:".dim(), stid.clone().cyan());
                    }
                    if !state.plan_execution_corrections.is_empty() {
                        eprintln!(
                            "  {} {} queued",
                            "Corrections:".dim(),
                            format!("{}", state.plan_execution_corrections.len()).yellow()
                        );
                    }
                    eprintln!("{}", "  pause | resume | show | help | exit".dim());
                } else {
                    let pct = ps.plan.progress_pct();
                    let done = ps.plan.items_done();
                    let total = ps.plan.subtasks.len();
                    let versions = ps.version_history.versions.len();
                    let edits = ps.history.len();

                    eprintln!("  {} {}", "Plan:".bold().cyan(), ps.goal);
                    let phase = if plan_idle_review_not_started(ps) {
                        format!("not started — type {} to run", "go".cyan())
                    } else {
                        "editing".to_string()
                    };
                    eprintln!("  {} {phase}", "Phase:".dim());

                    let bar = format_progress_bar(pct, 20);
                    eprintln!(
                        "  {} {bar} {}/{} {}  {} {}",
                        "Progress:".dim(),
                        format!("{done}").cyan(),
                        format!("{total}").dim(),
                        format!("({pct}%)").bold(),
                        format!("v{versions}").dim(),
                        format!("({edits} edits)").dim(),
                    );

                    let ready = ps.plan.ready_subtasks();
                    if !ready.is_empty() {
                        let ready_ids: Vec<_> = ready.iter().map(|st| st.id.as_str()).collect();
                        eprintln!("  {} {}", "Ready:".dim(), ready_ids.join(", ").cyan());
                    }

                    let blocked: Vec<_> = ps.plan.subtasks.iter()
                        .filter(|s| s.status == astra_services::task_orchestrator::TaskStatus::Pending
                            && !s.depends_on.is_empty()
                            && s.depends_on.iter().any(|dep| {
                                ps.plan.subtasks.iter().any(|d| d.id == *dep && d.status != astra_services::task_orchestrator::TaskStatus::Completed)
                            }))
                        .collect();
                    if !blocked.is_empty() {
                        for st in &blocked {
                            let deps: Vec<_> = st.depends_on.iter().map(|d| d.as_str()).collect();
                            eprintln!(
                                "  {} {} {}",
                                "●".dim(),
                                st.id.clone().yellow(),
                                format!("(waiting on: {})", deps.join(", ")).dim()
                            );
                        }
                    }

                    eprintln!("{}", "  execute | step | edit <…> | diff | history".dim());
                }
            } else if let Some(plan) = &state.executing_plan {
                let pct = plan.progress_pct();
                let done = plan.items_done();
                let total = plan.subtasks.len();
                let goal = state.executing_plan_goal.as_deref().unwrap_or("(unknown)");
                let round = state.plan_execution_rounds;

                eprintln!("  {} {goal}", "Plan:".bold().cyan());
                let bar = format_progress_bar(pct, 20);
                eprintln!(
                    "  {} {bar} {}/{} {}  {}",
                    "Progress:".dim(),
                    format!("{done}").cyan(),
                    format!("{total}").dim(),
                    format!("({pct}%)").bold(),
                    format!("round {round}").dim(),
                );

                if let Some(ref stid) = state.current_plan_subtask_id {
                    eprintln!("  {} {}", "Current:".dim(), stid.clone().cyan());
                }

                let corrections = &state.plan_execution_corrections;
                if !corrections.is_empty() {
                    eprintln!(
                        "  {} {} queued",
                        "Corrections:".dim(),
                        format!("{}", corrections.len()).yellow()
                    );
                }
                eprintln!("{}", "  pause | correct <…> | cancel".dim());
            } else {
                eprintln!("  {}", "No active plan or execution".dim());
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

            if plan_state.plan.subtasks.is_empty() {
                eprintln!(
                    "  {} Plan has no subtasks. Describe what you want to do first.",
                    theme::icon_warn()
                );
                return Ok(());
            }

            let plan = plan_state.plan.clone();
            let goal = plan_state.goal.clone();

            eprintln!();
            eprint_styled_execution_preview(&plan);
            eprintln!();

            if step_by_step {
                eprintln!(
                    "  {} Step-by-step mode: you'll confirm each subtask before execution.",
                    "⚙".cyan()
                );
                eprintln!(
                    "  {} Prompt shows {} while the run is active.",
                    "→".dim(),
                    "plan*[…]>".yellow()
                );
                eprintln!();
            }

            // Persist to task service
            state.plan_run_task_id = None;
            state.plan_run_task_last_progress = None;
            state.plan_run_task_last_error = None;
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
                        state.plan_run_task_id = Some(tid.clone());
                        let short = &tid[..8.min(tid.len())];
                        eprintln!(
                            "  {} {} {}",
                            theme::icon_ok(),
                            "Task created:".bold(),
                            format!("{} ({})", goal, short).dim()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "  {} Could not persist task: {}",
                            theme::icon_warn(),
                            e.to_string().yellow()
                        );
                    }
                }
            }

            if !step_by_step {
                eprintln!(
                    "  {} {} ({} subtasks)",
                    "▸".bold().cyan(),
                    "Auto-executing plan".bold(),
                    format!("{}", plan.subtasks.len()).cyan()
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
            state.executing_plan_goal = Some(goal.clone());
            if let Some(change) = super::repl_turn::steer_observability_goal(state, &goal) {
                journal_goal_steering_event(
                    &mut state.journal,
                    change.turn,
                    "plan_execution_start",
                    change.previous_goal.as_deref(),
                    &goal,
                    Some(serde_json::json!({
                        "mode": if step_by_step { "step_by_step" } else { "auto" },
                        "subtask_count": plan.subtasks.len(),
                    })),
                );
            }
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
                eprintln!(
                    "  {} {}",
                    theme::icon_warn(),
                    "No paused plan to resume".yellow()
                );
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
                eprintln!("  {}", "(no timeline data)".dim());
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

                eprintln!("{}", "Metrics".bold().cyan());
                eprintln!(
                    "  {} {}/{} {}  {} {}  {} events",
                    "Progress:".dim(),
                    format!("{done}").cyan(),
                    format!("{total}").dim(),
                    format!("({pct}%)").bold(),
                    format!("v{versions}").dim(),
                    format!("({edits} edits)").dim(),
                    timeline_events,
                );

                if !ps.plan.subtasks.is_empty() {
                    for st in &ps.plan.subtasks {
                        let icon = match st.status {
                            astra_services::task_orchestrator::TaskStatus::Completed => {
                                "✓".green().to_string()
                            }
                            astra_services::task_orchestrator::TaskStatus::Failed => {
                                "✗".red().to_string()
                            }
                            astra_services::task_orchestrator::TaskStatus::InProgress => {
                                "▶".cyan().to_string()
                            }
                            _ => "○".dim().to_string(),
                        };
                        let deps = if st.depends_on.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", format!("(deps: {})", st.depends_on.join(", ")).dim())
                        };
                        eprintln!("  {icon} {} {}{deps}", st.id.clone().cyan(), st.title);
                    }
                }
            } else {
                eprintln!("  {}", "No active plan for metrics".dim());
            }
        }

        PlanCommand::History => {
            if let Some(ref ps) = state.plan_mode {
                eprintln!("{}", ps.version_history.format_log());
            } else {
                eprintln!("  {}", "No version history".dim());
            }
        }

        PlanCommand::Show => {
            if let Some(ref ps) = state.plan_mode {
                eprintln!();
                eprint_plan_markdown_streaming(&ps.plan, Some(&ps.goal));
            } else {
                eprintln!("  {}", "No active plan".dim());
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
                        eprint_plan_markdown_streaming(&ps.plan, Some(&ps.goal));
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
                            count.to_string().cyan(),
                            anchor.cyan(),
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
                eprintln!(
                    "  {} {}",
                    theme::icon_warn(),
                    "Rewind is only available during execution".yellow()
                );
            }
        }

        PlanCommand::EnablePlanOnly => {
            state.chat_plan_only = true;
            eprintln!(
                "  {} Plan-only chat enabled (tools disabled)",
                theme::icon_ok()
            );
        }

        PlanCommand::DisablePlanOnly => {
            state.chat_plan_only = false;
            eprintln!(
                "  {} Plan-only chat disabled (tools re-enabled)",
                theme::icon_ok()
            );
        }

        PlanCommand::Help => {
            let goal = state
                .plan_mode
                .as_ref()
                .map(|ps| ps.goal.as_str())
                .unwrap_or("");
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
        PendingClarifications, PlanModeState, decomposition_prompt, detect_clarification_questions,
        format_project_context, parse_plan_response,
    };

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
        return Ok(());
    };

    let Some(plan_state) = state.plan_mode.as_mut() else {
        return Ok(());
    };
    plan_state.goal = goal.clone();

    journal_plan_event(
        &mut state.journal,
        session_journal::JournalEventType::PlanLifecycle,
        &format!(
            "Plan mode started: {}",
            goal.chars().take(80).collect::<String>()
        ),
        Some(serde_json::json!({
            "goal": goal,
        })),
    );

    if state.verbose_mode {
        eprintln!();
        eprintln!("{}", format_project_context(&plan_state.context).dim());
    }

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

    eprintln!();
    let resp = api.post_chat_turn(tok, &payload).await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let sse_result = collect_sse_with_preview(r).await;
            if let Some(err) = sse_result.completion_error() {
                eprintln!("  {} {}", theme::icon_err(), err.red());
                return Ok(());
            }
            let full_text = sse_result.text;

            if let Some(questions) = detect_clarification_questions(&full_text) {
                eprintln!();
                eprintln!(
                    "  {} {}",
                    "▸".cyan(),
                    format!(
                        "{} question{} before planning:",
                        questions.len(),
                        if questions.len() == 1 { "" } else { "s" }
                    )
                    .bold()
                    .cyan()
                );
                eprintln!();

                let Some(plan_state) = state.plan_mode.as_mut() else {
                    return Ok(());
                };
                let pending = PendingClarifications {
                    questions: questions.clone(),
                    answers: Vec::new(),
                };
                plan_state.pending_clarifications = Some(pending);

                eprint_clarification_question(&questions[0]);
                let _ = plan_state.save_to_file(&PlanModeState::state_path());
            } else {
                match parse_plan_response(&full_text) {
                    Ok(plan) => {
                        let Some(plan_state) = state.plan_mode.as_mut() else {
                            return Ok(());
                        };
                        plan_state.set_plan(plan);
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());

                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanLifecycle,
                            &format!(
                                "Plan generated: {} subtasks",
                                plan_state.plan.subtasks.len()
                            ),
                            Some(serde_json::json!({
                                "subtask_count": plan_state.plan.subtasks.len(),
                            })),
                        );

                        let subtask_count = plan_state.plan.subtasks.len();
                        eprint_plan_commands_help();

                        // Offer interactive confirmation if terminal supports it
                        if let Some(choice) = prompt_plan_confirmation(subtask_count) {
                            match choice {
                                PlanConfirmChoice::ExecuteAll => {
                                    return Box::pin(handle_plan_mode_input(
                                        "go".into(),
                                        token,
                                        state,
                                        api,
                                    ))
                                    .await;
                                }
                                PlanConfirmChoice::StepByStep => {
                                    return Box::pin(handle_plan_mode_input(
                                        "step".into(),
                                        token,
                                        state,
                                        api,
                                    ))
                                    .await;
                                }
                                PlanConfirmChoice::Edit | PlanConfirmChoice::Cancel => {}
                            }
                        }
                    }
                    Err(e) => {
                        eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    }
                }
            }
        }
        Ok(r) => {
            cli_utils::eprint_api_error(r.status().as_u16(), "LLM call failed");
        }
        Err(e) => {
            cli_utils::eprint_request_error(&e);
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
        assert!(!plan_execution_ui_active(&state), "no handle => inactive");

        let mut state = ReplState::default();
        let (handle, _update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        assert!(plan_execution_ui_active(&state), "handle present => active");
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
    fn try_replace_plan_from_llm_json_empty_text_no_op() {
        let mut ps = plan::PlanModeState::new("g".into(), plan::ProjectContext::default());
        assert!(!try_replace_plan_from_llm_json("", &mut ps).unwrap());
        assert!(!try_replace_plan_from_llm_json("   \n\t  ", &mut ps).unwrap());
    }

    #[test]
    fn try_replace_plan_from_llm_json_prose_is_no_op() {
        let mut ps = plan::PlanModeState::new("g".into(), plan::ProjectContext::default());
        // Pure prose with no JSON → Ok(false), not an error
        assert_eq!(
            try_replace_plan_from_llm_json("Just use natural language.", &mut ps).unwrap(),
            false
        );
    }

    #[test]
    fn try_replace_plan_from_llm_json_bad_json_returns_err() {
        let mut ps = plan::PlanModeState::new("g".into(), plan::ProjectContext::default());
        // Has `{` but invalid JSON → real parse error
        let err =
            try_replace_plan_from_llm_json("Here is the plan: {broken", &mut ps).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn try_replace_plan_from_llm_json_valid_plan_replaces() {
        let mut ps = plan::PlanModeState::new("g".into(), plan::ProjectContext::default());
        let json = r#"{"subtasks":[{"id":"t1","title":"step"}]}"#;
        assert!(try_replace_plan_from_llm_json(json, &mut ps).unwrap());
        assert_eq!(ps.plan.subtasks.len(), 1);
        assert_eq!(ps.plan.subtasks[0].id, "t1");
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

    #[test]
    fn try_replace_preserves_completed_subtasks_when_llm_drops_them() {
        let mut ps =
            plan::PlanModeState::new("build login".into(), plan::ProjectContext::default());
        // Simulate a completed plan
        ps.set_plan(astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "html".into(),
                    title: "Create HTML".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "css".into(),
                    title: "Add CSS".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
            ],
            notes: None,
        });

        // LLM returns only the new subtask, dropping completed ones
        let json = r#"{"subtasks":[{"id":"move-files","title":"Move files to directory"}]}"#;
        assert!(try_replace_plan_from_llm_json(json, &mut ps).unwrap());

        // All three subtasks should be present
        assert_eq!(ps.plan.subtasks.len(), 3);
        // Completed ones preserved at front
        assert_eq!(ps.plan.subtasks[0].id, "html");
        assert_eq!(ps.plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(ps.plan.subtasks[1].id, "css");
        assert_eq!(ps.plan.subtasks[1].status, TaskStatus::Completed);
        // New one appended
        assert_eq!(ps.plan.subtasks[2].id, "move-files");
        assert_eq!(ps.plan.subtasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn try_replace_preserves_status_when_llm_resets_completed_to_pending() {
        let mut ps =
            plan::PlanModeState::new("build login".into(), plan::ProjectContext::default());
        ps.set_plan(astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "html".into(),
                title: "Create HTML".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            }],
            notes: None,
        });

        // LLM includes the old subtask but resets status to pending
        let json =
            r#"{"subtasks":[{"id":"html","title":"Create HTML"},{"id":"new","title":"New task"}]}"#;
        assert!(try_replace_plan_from_llm_json(json, &mut ps).unwrap());

        assert_eq!(ps.plan.subtasks.len(), 2);
        assert_eq!(ps.plan.subtasks[0].id, "html");
        assert_eq!(ps.plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(ps.plan.subtasks[1].id, "new");
        assert_eq!(ps.plan.subtasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn try_replace_no_protection_when_nothing_completed() {
        let mut ps = plan::PlanModeState::new("goal".into(), plan::ProjectContext::default());
        ps.set_plan(astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "old".into(),
                title: "Old".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        });

        // Full replacement is fine when nothing was completed
        let json = r#"{"subtasks":[{"id":"new","title":"Replacement"}]}"#;
        assert!(try_replace_plan_from_llm_json(json, &mut ps).unwrap());
        assert_eq!(ps.plan.subtasks.len(), 1);
        assert_eq!(ps.plan.subtasks[0].id, "new");
    }

    #[test]
    fn try_replace_plan_from_llm_json_empty_subtasks_replaces() {
        let ctx = plan::ProjectContext::default();
        let mut ps = plan::PlanModeState::new("test".into(), ctx);
        ps.set_plan(astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "old".into(),
                title: "Old".into(),
                ..Default::default()
            }],
            notes: None,
        });
        // LLM returns valid JSON with empty subtasks — should still replace
        let json = r#"{"subtasks": []}"#;
        assert!(try_replace_plan_from_llm_json(json, &mut ps).unwrap());
        assert!(ps.plan.subtasks.is_empty());
    }

    #[test]
    fn is_execute_command_matches_go_and_variants() {
        assert!(plan::PlanModeState::is_execute_command("go"));
        assert!(plan::PlanModeState::is_execute_command("  GO  "));
        assert!(plan::PlanModeState::is_execute_command("execute"));
        assert!(plan::PlanModeState::is_execute_command("开始"));
        assert!(!plan::PlanModeState::is_execute_command("show"));
        assert!(!plan::PlanModeState::is_execute_command(""));
    }
}
