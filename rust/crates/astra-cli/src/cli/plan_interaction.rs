//! Plan mode interaction handler — extracted from `slash_memory.rs`.
//!
//! Handles all user input while in the interactive plan editing mode (`plan>` prompt).
//! Uses the `PlanCommand` parser for structured commands and falls back to
//! natural-language plan editing via LLM.

use super::*;
use crate::sse_utils::collect_sse_cancellable;
use astra_runtime::plan;
use astra_runtime::plan::PlanCommand;
use astra_runtime::plan::progress_bar_segments;
use astra_services::session_journal;
use crossterm::style::Stylize;

/// Default timeouts for plan LLM calls.
const PLAN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const PLAN_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const PLAN_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Outcome of a cancellable plan LLM call.
enum PlanLlmOutcome {
    /// LLM returned text successfully.
    Ok {
        text: String,
        session_id: Option<String>,
    },
    /// User pressed Ctrl-C during generation.
    Cancelled,
    /// HTTP or SSE error.
    Error(String),
}

/// If the LLM response contains a session_id and we don't have one yet, initialize it.
fn maybe_init_session_from_plan(state: &mut ReplState, outcome: &PlanLlmOutcome) {
    if let PlanLlmOutcome::Ok {
        session_id: Some(sid),
        ..
    } = outcome
    {
        // Always adopt the server-created session_id. The local session_id
        // (generated at plan entry for journal) may differ from the server's.
        if state.session_id.as_deref() != Some(sid.as_str()) {
            super::repl_turn::initialize_journal_pub(state, sid);
            state.session_id = Some(sid.clone());
        }
    }
}

/// Send a plan LLM request with Ctrl-C cancellation and timeouts.
///
/// Wraps `post_chat_turn_timeout` + `collect_sse_cancellable` in a
/// `tokio::select!` that listens for `ctrl_c()`. On interrupt the
/// cancellation token is fired, the SSE stream is drained, and
/// `PlanLlmOutcome::Cancelled` is returned.
async fn plan_llm_call(
    api: &astra_thin_client::ThinClient,
    token: &str,
    payload: &serde_json::Value,
) -> PlanLlmOutcome {
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_for_signal = cancel.clone();

    // Race the full request+stream against Ctrl-C.
    // Once ctrl_c fires we cancel the token; the inner select in
    // collect_sse_cancellable will observe it and break out.
    let result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            cancel_for_signal.cancel();
            // Give the stream a moment to drain cleanly
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return PlanLlmOutcome::Cancelled;
        }
        r = async {
            // Retry up to 3 times on SSE transport errors (e.g. "error decoding response body").
            for attempt in 0..3u8 {
                let resp = match api.post_chat_turn_timeout(token, payload, PLAN_REQUEST_TIMEOUT).await {
                    Ok(r) => r,
                    Err(e) => return PlanLlmOutcome::Error(e.to_string()),
                };
                if !resp.status().is_success() {
                    return PlanLlmOutcome::Error(format!("HTTP {}", resp.status()));
                }
                let sse_result = collect_sse_cancellable(
                    resp,
                    &cancel,
                    PLAN_STREAM_TIMEOUT,
                    PLAN_IDLE_TIMEOUT,
                    |_| {},
                ).await;
                if sse_result.is_cancelled() {
                    return PlanLlmOutcome::Cancelled;
                }
                if let Some(err) = sse_result.completion_error() {
                    if attempt < 2 {
                        eprintln!("  {} SSE error, retrying… ({}/3, {})", crate::theme::icon_warn(), attempt + 1, err);
                        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1))).await;
                        continue;
                    }
                    return PlanLlmOutcome::Error(err);
                }
                return PlanLlmOutcome::Ok { text: sse_result.text, session_id: sse_result.session_id };
            }
            PlanLlmOutcome::Error("SSE retries exhausted".into())
        } => r,
    };
    result
}

/// Generate a plan from a goal, with automatic retry on JSON parse failure.
///
/// On first attempt, sends the decomposition prompt. If the LLM returns text
/// that fails JSON parsing, sends a correction prompt and retries once.
async fn plan_generate_with_retry(
    api: &astra_thin_client::ThinClient,
    token: &str,
    goal: &str,
    context: &plan::ProjectContext,
    _session_id: Option<&str>,
) -> PlanLlmOutcome {
    use astra_runtime::plan::decomposition_prompt;

    let prompt = decomposition_prompt(goal, context);
    let payload = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
    });

    let result = plan_llm_call(api, token, &payload).await;
    let text = match &result {
        PlanLlmOutcome::Ok { text: t, .. } => t,
        _ => return result,
    };

    // Check if it parses — if yes, return immediately
    if plan::parse_plan_response(text).is_ok()
        || plan::detect_clarification_questions(text).is_some()
    {
        return result;
    }

    // Parse failed — retry with correction prompt
    eprintln!(
        "  {} JSON parse failed, retrying with correction…",
        theme::icon_warn()
    );

    let retry_prompt = format!(
        "Your previous response was not valid JSON. Please output ONLY a JSON object \
         with this exact structure:\n\
         {{\"subtasks\": [{{\"id\": \"...\", \"title\": \"...\", \"description\": \"...\", \
         \"depends_on\": [], \"effort\": \"small|medium|large\", \"files\": [\"...\"]}}]}}\n\n\
         No markdown, no explanation, no code fences. Just the raw JSON.\n\n\
         Original goal: {goal}"
    );
    let retry_payload = serde_json::json!({
        "messages": [
            {"role": "user", "content": prompt},
            {"role": "assistant", "content": text},
            {"role": "user", "content": retry_prompt},
        ],
    });

    plan_llm_call(api, token, &retry_payload).await
}

/// Result from [`handle_plan_mode_input`].
///
/// Using an enum instead of Err("__SEND_AS_CHAT__:…") provides type-safe
/// communication between the plan handler and the main REPL loop.
#[derive(Debug)]
pub enum PlanInputResult {
    /// Input was fully handled within plan mode.
    Handled,
    /// Re-dispatch the enclosed slash command through the main REPL slash handler.
    DispatchSlash(String),
    /// Plan was abandoned; the enclosed message should be sent as normal chat.
    SendAsChat(String),
}

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
/// Styled plan display with crossterm colors — replaces markdown streaming for plan confirmation.
fn eprint_styled_plan(plan: &astra_runtime::plan::TaskPlan, goal: &str) {
    use astra_runtime::plan::TaskStatus;

    eprintln!("  {} {}", "Plan:".bold(), goal.cyan());
    if let Some(ref notes) = plan.notes {
        eprintln!("  {}", notes.as_str().dim());
    }
    eprintln!();

    for (i, st) in plan.subtasks.iter().enumerate() {
        let (icon, icon_style) = match st.status {
            TaskStatus::Completed => ("✓", "green"),
            TaskStatus::InProgress => ("▶", "cyan"),
            TaskStatus::Failed => ("✗", "red"),
            TaskStatus::Paused => ("⏸", "yellow"),
            _ => ("○", "dim"),
        };
        let icon_str = match icon_style {
            "green" => icon.green().to_string(),
            "cyan" => icon.cyan().to_string(),
            "red" => icon.red().to_string(),
            "yellow" => icon.yellow().to_string(),
            _ => icon.dim().to_string(),
        };

        let effort = match st.effort.as_deref() {
            Some("small") => " S".green().to_string(),
            Some("medium") => " M".yellow().to_string(),
            Some("large") => " L".red().to_string(),
            _ => String::new(),
        };

        eprintln!(
            "  {} {} {}{}  {}",
            format!("{:>2}.", i + 1).dim(),
            icon_str,
            st.id.as_str().bold(),
            effort,
            st.title.as_str().dim()
        );

        if let Some(ref desc) = st.description {
            let short: String = desc.chars().take(80).collect();
            let suffix = if desc.len() > 80 { "…" } else { "" };
            eprintln!("      {}{}", short.dim(), suffix.dim());
        }

        if !st.files.is_empty() {
            let files: Vec<_> = st
                .files
                .iter()
                .take(4)
                .map(|f| f.as_str().dim().to_string())
                .collect();
            let suffix = if st.files.len() > 4 {
                format!(" +{}", st.files.len() - 4)
            } else {
                String::new()
            };
            eprintln!("      {} {}{}", "📁".dim(), files.join(", "), suffix.dim());
        }

        if !st.depends_on.is_empty() {
            eprintln!(
                "      {} {}",
                "↳".dim(),
                format!("after {}", st.depends_on.join(", ")).dim()
            );
        }
    }

    // Summary line
    let done = plan.items_done();
    let total = plan.subtasks.len();
    let pct = plan.progress_pct();
    eprintln!();
    eprintln!(
        "  {} {}/{} ({}%)",
        format_progress_bar(pct, 15),
        format!("{done}").cyan(),
        format!("{total}").dim(),
        pct
    );
}

/// Styled outline display with crossterm colors.
fn eprint_styled_outline(outline: &astra_runtime::plan::outline::PlanOutline, goal: &str) {
    let effort_styled = match outline.total_effort.as_str() {
        "small" => "small".green().to_string(),
        "medium" => "medium".yellow().to_string(),
        "large" => "large".red().to_string(),
        other => other.to_string(),
    };

    eprintln!("  {} {}", "Plan:".bold(), goal.cyan());
    eprintln!(
        "  {} {}  ·  {} phase{}",
        "Effort:".bold(),
        effort_styled,
        format!("{}", outline.phases.len()).cyan(),
        if outline.phases.len() == 1 { "" } else { "s" }
    );
    eprintln!();

    for (i, phase) in outline.phases.iter().enumerate() {
        eprintln!(
            "  {} {} — {}",
            format!("{}.", i + 1).bold().cyan(),
            phase.title.as_str().bold(),
            phase.description.as_str().dim()
        );
        let mut detail = format!(
            "     ~{} subtask{}",
            format!("{}", phase.estimated_subtasks).cyan(),
            if phase.estimated_subtasks == 1 {
                ""
            } else {
                "s"
            }
        );
        if !phase.key_files.is_empty() {
            let files: Vec<_> = phase
                .key_files
                .iter()
                .take(3)
                .map(|f| f.as_str().dim().to_string())
                .collect();
            detail.push_str(&format!("  ·  {}", files.join(", ")));
        }
        eprintln!("{detail}");
    }
    eprintln!();
}

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
        astra_runtime::plan_decompose::ClarificationCategory::Other => "💬",
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
    eprint!("{}", "📋 Plan mode".yellow().bold());
    if !goal.is_empty() {
        let display_goal: String = goal.chars().take(60).collect();
        let suffix = if goal.len() > 60 { "…" } else { "" };
        eprint!(" — {}{}", display_goal.cyan(), suffix);
    }
    eprintln!();
    eprintln!(
        "{}",
        "  Type a goal to start · go to execute · show to view · exit to leave".dim()
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

/// B6: Drop the in-memory plan_mode state and delete persisted state files
/// after a generation failure or cancellation.
///
/// Without this, a cancelled outline left `state.plan_mode = Some(...)` with
/// `plan.subtasks` empty. The user would then type "继续" expecting it to
/// retry generation, but `PlanCommand::parse` routed it to Resume which —
/// even after B4's EmptyNoSubtasks branch — still left them in plan-mode
/// purgatory rather than back in normal chat.
///
/// We deliberately keep the journal event so the failure is auditable.
fn abort_plan_mode_after_failure(state: &mut ReplState, stage: &'static str, reason: &str) {
    journal_plan_event(
        &mut state.journal,
        session_journal::JournalEventType::PlanLifecycle,
        &format!("Plan generation aborted at {stage}: {reason}"),
        Some(serde_json::json!({
            "stage": stage,
            "reason": reason,
            "outcome": "abort",
        })),
    );
    state.plan_mode = None;
    state.chat_plan_only = false;
    state.pending_plan_resume_digest = None;
    let path = astra_runtime::plan_decompose::PlanModeState::state_path();
    let _ = astra_runtime::plan_decompose::PlanModeState::clear_saved_state_at(&path);
}

/// P2: Single-stage analytical-plan generation.
///
/// Runs one LLM call against `astra_plan::analytical::analytical_prompt`,
/// renders the resulting `ResearchPlan`, and tears down plan_mode. Unlike
/// the executable flow there is no outline/confirm/expand loop and no
/// executor invocation — the deliverable is the rendered plan itself.
async fn handle_analytical_goal(
    goal: String,
    tok: &str,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    let context = state
        .plan_mode
        .as_ref()
        .map(|ps| ps.context.clone())
        .unwrap_or_default();

    let prompt = astra_runtime::plan::analytical::analytical_prompt(&goal, &context);
    let payload = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
    });

    eprintln!();
    eprintln!("  {} Analyzing question…", "⋯".dim());

    let result = plan_llm_call(api, tok, &payload).await;
    maybe_init_session_from_plan(state, &result);
    let text = match result {
        PlanLlmOutcome::Ok { text, .. } => text,
        PlanLlmOutcome::Cancelled => {
            eprintln!("  {} Analytical plan cancelled.", theme::icon_warn());
            abort_plan_mode_after_failure(state, "analytical", "cancelled");
            return Ok(PlanInputResult::Handled);
        }
        PlanLlmOutcome::Error(e) => {
            eprintln!("  {} {}", theme::icon_err(), e.clone().red());
            abort_plan_mode_after_failure(state, "analytical", &e);
            return Ok(PlanInputResult::Handled);
        }
    };

    match astra_runtime::plan::analytical::parse_analytical_response(&text) {
        Ok(plan) => {
            eprintln!();
            eprintln!(
                "{}",
                astra_runtime::plan::analytical::format_research_plan(&plan)
            );
            journal_plan_event(
                &mut state.journal,
                session_journal::JournalEventType::PlanLifecycle,
                &format!(
                    "Analytical plan delivered ({} questions)",
                    plan.questions.len()
                ),
                Some(serde_json::json!({
                    "stage": "analytical_delivered",
                    "kind": "analytical",
                    "question_count": plan.questions.len(),
                    "outcome": "ok",
                })),
            );
            // Analytical is one-shot: drop plan_mode so the user falls
            // back into normal chat to discuss any of the sub-questions.
            state.plan_mode = None;
            state.chat_plan_only = false;
            state.pending_plan_resume_digest = None;
            let path = astra_runtime::plan_decompose::PlanModeState::state_path();
            let _ = astra_runtime::plan_decompose::PlanModeState::clear_saved_state_at(&path);
            Ok(PlanInputResult::Handled)
        }
        Err(e) => {
            eprintln!(
                "  {} Failed to parse analytical plan: {}",
                theme::icon_err(),
                e.clone().red()
            );
            abort_plan_mode_after_failure(state, "analytical", &e);
            Ok(PlanInputResult::Handled)
        }
    }
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
///
/// Returns [`PlanInputResult::Handled`] when input was processed within plan mode,
/// [`PlanInputResult::DispatchSlash`] when slash input should be re-dispatched
/// through the main REPL slash handler, or [`PlanInputResult::SendAsChat`] when
/// the plan was abandoned and the message should be sent as normal chat.
pub async fn handle_plan_mode_input(
    input: String,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use plan::{
        ClarificationAnswer, PlanEntryChoice, PlanModeState, decomposition_prompt,
        parse_clarification_response, parse_plan_entry_choice, parse_plan_response,
    };

    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  {} {}", theme::icon_warn(), "Not in plan mode".yellow());
            return Ok(PlanInputResult::Handled);
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
                return Ok(PlanInputResult::Handled);
            }
        }

        if let Some(next_q) = pending.next_question() {
            eprintln!();
            eprint_clarification_question(next_q);
            let _ = plan_state.save_to_file(&PlanModeState::state_path());
            return Ok(PlanInputResult::Handled);
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
            return Ok(PlanInputResult::Handled);
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
        });

        eprintln!();
        let plan_result = plan_llm_call(api, tok, &payload).await;
        // Extract session_id as owned value — plan_state holds a mutable borrow of
        // state.plan_mode, so we can't call maybe_init_session_from_plan(state, ..) directly.
        let new_session_id: Option<String> = if let PlanLlmOutcome::Ok {
            session_id: Some(ref sid),
            ..
        } = plan_result
        {
            if state.session_id.is_none() {
                Some(sid.clone())
            } else {
                None
            }
        } else {
            None
        };
        let full_text = match plan_result {
            PlanLlmOutcome::Ok { text, .. } => text,
            PlanLlmOutcome::Cancelled => {
                eprintln!("  {} Plan generation cancelled.", theme::icon_warn());
                return Ok(PlanInputResult::Handled);
            }
            PlanLlmOutcome::Error(e) => {
                eprintln!("  {} {}", theme::icon_err(), e.red());
                return Ok(PlanInputResult::Handled);
            }
        };

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

        // Apply session_id captured from the LLM response (plan_state borrow ends here).
        if let Some(sid) = new_session_id {
            super::repl_turn::initialize_journal_pub(state, &sid);
            state.session_id = Some(sid);
        }
        return Ok(PlanInputResult::Handled);
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
                return Ok(PlanInputResult::Handled);
            }
            PlanEntryChoice::Continue => {
                eprintln!("  {} Continuing with current plan", "→".cyan());
                return Ok(PlanInputResult::Handled);
            }
            PlanEntryChoice::Restart => {
                plan_state.plan = Default::default();
                plan_state.goal = String::new();
                eprintln!(
                    "  {} Plan cleared. Describe what you want to do:",
                    "🔄".yellow()
                );
                return Ok(PlanInputResult::Handled);
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
                return Ok(PlanInputResult::Handled);
            }
            PlanEntryChoice::New(_) => {
                plan_state.plan = Default::default();
                eprintln!("  {} Describe what you want to do:", "📝".cyan());
                return Ok(PlanInputResult::Handled);
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
            return Ok(PlanInputResult::Handled);
        }
        let Some(plan_state) = state.plan_mode.as_mut() else {
            return Ok(PlanInputResult::Handled);
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
        return Ok(PlanInputResult::Handled);
    }

    // ── Natural-language plan editing via LLM ───────────────────────────
    if plan_execution_ui_active(state) {
        // When plan is paused (handle exists but monitor exited), route input through
        // PlanCommand parser first. If it's a valid command (continue, status, etc.),
        // execute it. If it's a /slash command, re-dispatch it through the main REPL
        // slash handler. Otherwise, abandon the plan per documented behavior:
        // "Any other message — abandons the plan and sends it as a normal chat turn"

        // First check for valid PlanCommand (continue, status, exit, etc.)
        if let Some(cmd) = PlanCommand::parse(&input) {
            return handle_plan_command(cmd, token, state, api).await;
        }

        // Re-dispatch /slash commands through the main REPL slash handler.
        if input.starts_with('/') {
            return Ok(PlanInputResult::DispatchSlash(input));
        }

        // Not a command, not a slash — abandon plan and send as chat
        if let Some(ref handle) = state.plan_handle {
            if let Err(e) = handle.send_command(crate::plan_executor::PlanCommand::Cancel) {
                astra_core::agent_warn!(
                    "plan",
                    "failed to send Cancel to executor during abandon: {e}"
                );
            }
        }
        state.plan_handle = None;
        state.plan_mode = None;
        state.executing_plan = None;
        eprintln!(
            "  {} Plan abandoned. Sending as normal chat...",
            theme::icon_warn()
        );
        // Return typed result to signal caller should handle as chat
        return Ok(PlanInputResult::SendAsChat(input));
    }
    let Some(plan_state) = state.plan_mode.as_mut() else {
        return Ok(PlanInputResult::Handled);
    };

    let prompt = plan_state.plan_mode_prompt(&input);
    plan_state.add_turn(&input, "");

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
        return Ok(PlanInputResult::Handled);
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
    let edit_result = plan_llm_call(api, tok, &payload).await;
    maybe_init_session_from_plan(state, &edit_result);
    let llm_text = match edit_result {
        PlanLlmOutcome::Ok { text, .. } => text,
        PlanLlmOutcome::Cancelled => {
            eprintln!("  {} Plan edit cancelled.", theme::icon_warn());
            return Ok(PlanInputResult::Handled);
        }
        PlanLlmOutcome::Error(e) => {
            eprintln!("  {} {}", theme::icon_err(), e.red());
            return Ok(PlanInputResult::Handled);
        }
    };

    if llm_text.is_empty() {
        eprintln!("  {} No response from server", theme::icon_warn());
    }

    if !llm_text.is_empty() {
        let Some(plan_state) = state.plan_mode.as_mut() else {
            return Ok(PlanInputResult::Handled);
        };
        match try_replace_plan_from_llm_json(&llm_text, plan_state) {
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
                    .filter(|s| s.status == astra_services::task_orchestrator::TaskStatus::Pending)
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
            last.1 = llm_text.chars().take(500).collect();
        }
    }

    Ok(PlanInputResult::Handled)
}

// ─── Plan Resume Recovery ────────────────────────────────────────────────────

/// Result of attempting to recover a plan for resume after executor failure.
#[derive(Debug)]
enum PlanResumeRecovery {
    /// Plan has work remaining — Failed subtasks reset to Pending. Contains the
    /// recovered plan ready for a new executor.
    Ready(astra_services::task_orchestrator::TaskPlan),
    /// All subtasks are already Completed — nothing to resume.
    AllCompleted,
    /// Plan has no subtasks at all — generation never produced any work.
    /// (Previously misreported as `NothingToDo`/AllCompleted, surfacing the
    /// confusing "All subtasks already completed" message on a fresh empty
    /// plan after a failed generation.)
    EmptyNoSubtasks,
}

/// Recover a plan for resume: reset Failed→Pending and return a clone if work remains.
///
/// Called when the executor is gone (PlanError) but `plan_mode` still holds the
/// plan with up-to-date subtask statuses (via `SubtaskStatusSync`).
fn recover_plan_for_resume(
    plan: &mut astra_services::task_orchestrator::TaskPlan,
) -> PlanResumeRecovery {
    use astra_services::task_orchestrator::TaskStatus;

    if plan.subtasks.is_empty() {
        return PlanResumeRecovery::EmptyNoSubtasks;
    }

    let has_work = plan
        .subtasks
        .iter()
        .any(|s| s.status == TaskStatus::Pending || s.status == TaskStatus::Failed);
    if !has_work {
        return PlanResumeRecovery::AllCompleted;
    }
    for st in &mut plan.subtasks {
        if st.status == TaskStatus::Failed {
            st.status = TaskStatus::Pending;
        }
    }
    PlanResumeRecovery::Ready(plan.clone())
}

/// Handle a parsed `PlanCommand`.
async fn handle_plan_command(
    cmd: PlanCommand,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use plan::{PlanExecutionConfig, PlanModeState};

    match cmd {
        PlanCommand::Cancel => {
            shutdown_plan_executor(state);

            journal_plan_event(
                &mut state.journal,
                session_journal::JournalEventType::PlanLifecycle,
                "Plan mode cancelled",
                Some(serde_json::json!({ "stage": "cancelled" })),
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
                None => return Ok(PlanInputResult::Handled),
            };

            if plan_execution_ui_active(state) {
                eprintln!(
                    "  {} A plan is already running. Wait for it to finish, or use {} / {}.",
                    theme::icon_warn(),
                    "pause".cyan(),
                    "exit".cyan()
                );
                return Ok(PlanInputResult::Handled);
            }

            if plan_state.plan.subtasks.is_empty() {
                eprintln!(
                    "  {} Plan has no subtasks. Describe what you want to do first.",
                    theme::icon_warn()
                );
                return Ok(PlanInputResult::Handled);
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
                    "  {} The run continues in background; approvals and status stay available on the normal prompt.",
                    "→".dim(),
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
            state.plan_mode = None;
            state.chat_plan_only = false;
            state.pending_plan_resume_digest = None;
            PlanModeState::clear_saved_state();
            eprintln!(
                "  {} Left plan mode — execution is now running in background. Use {} to inspect it.",
                "←".cyan(),
                "/plan status".cyan()
            );
        }

        PlanCommand::Resume => {
            if let Some(ref handle) = state.plan_handle {
                // Executor is alive (paused) — send Resume command.
                let corrections = if state.plan_execution_corrections.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut state.plan_execution_corrections))
                };
                match handle.send_command(crate::plan_executor::PlanCommand::Resume { corrections })
                {
                    Ok(()) => {
                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanLifecycle,
                            "Plan execution resumed",
                            Some(serde_json::json!({ "stage": "resumed" })),
                        );
                        eprintln!(
                            "  {} Resuming plan execution in background. Use {} for progress.",
                            "▶".cyan(),
                            "/plan status".cyan()
                        );
                    }
                    Err(e) => eprintln!("  {} {}", theme::icon_err(), e),
                }
            } else if let Some(ref mut ps) = state.plan_mode {
                match recover_plan_for_resume(&mut ps.plan) {
                    PlanResumeRecovery::Ready(plan) => {
                        state.executing_plan = Some(plan);
                        eprintln!("  {} Resuming plan execution...", "▶".cyan());
                    }
                    PlanResumeRecovery::AllCompleted => {
                        eprintln!(
                            "  {} {}",
                            theme::icon_warn(),
                            "All subtasks already completed — nothing to resume".yellow()
                        );
                    }
                    PlanResumeRecovery::EmptyNoSubtasks => {
                        // Plan generation never produced subtasks (likely
                        // failed JSON parse or cancelled). Don't lie that
                        // everything is "completed" — guide the user back.
                        eprintln!(
                            "  {} {}",
                            theme::icon_warn(),
                            "Plan is empty — type a goal to generate one, or 'cancel' to exit plan mode".yellow()
                        );
                    }
                }
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
                    Ok(()) => {
                        journal_plan_event(
                            &mut state.journal,
                            session_journal::JournalEventType::PlanLifecycle,
                            "Plan execution pause requested",
                            Some(serde_json::json!({ "stage": "pause_requested" })),
                        );
                        eprintln!("  {} Pause requested.", "⏸".cyan());
                    }
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
                return Ok(PlanInputResult::Handled);
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
                return Ok(PlanInputResult::Handled);
            }
            return Box::pin(handle_plan_mode_input(instruction, token, state, api)).await;
        }
    }

    Ok(PlanInputResult::Handled)
}

/// Handle initial goal submission — scan project and generate plan via LLM.
///
/// Uses a two-stage flow:
/// 1. Generate outline (2-4 phases) — fast, gives user a chance to review
/// 2. User confirms → expand each phase into subtasks
///
/// Falls back to direct full-plan generation if outline parsing fails.
async fn handle_goal_submission(
    goal: String,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use astra_runtime::plan::outline;
    use plan::{detect_clarification_questions, format_project_context, parse_plan_response};

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
        return Ok(PlanInputResult::Handled);
    };

    if state.plan_mode.is_none() {
        return Ok(PlanInputResult::Handled);
    }

    // B10: Initialise the journal writer eagerly so the "Plan mode started"
    // event and any subsequent abort events are recorded even if the very
    // first LLM call fails. Previously the journal writer was only attached
    // when an LLM response carried a session_id, so a failure on the first
    // outline call would silently drop every plan-mode event.
    if state.session_id.is_none() {
        let new_sid = uuid::Uuid::new_v4().to_string();
        super::repl_turn::initialize_journal_pub(state, &new_sid);
        state.session_id = Some(new_sid);
    } else if state.journal.is_none() {
        if let Some(sid) = state.session_id.clone() {
            super::repl_turn::initialize_journal_pub(state, &sid);
        }
    }

    let plan_state = state
        .plan_mode
        .as_mut()
        .expect("plan_mode is_some checked above");
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
            "stage": "entered",
            // P5: classify the goal at entry so downstream digesters can
            // distinguish executable vs analytical without re-running the
            // heuristic.
            "kind": match astra_runtime::plan_decompose::classify_plan_suggestion(&goal)
                .map(|s| s.kind)
                .unwrap_or(astra_runtime::plan_decompose::PlanKind::Executable)
            {
                astra_runtime::plan_decompose::PlanKind::Executable => "executable",
                astra_runtime::plan_decompose::PlanKind::Analytical => "analytical",
            },
            "started_at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
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

    // P2: If the goal classifies as an analytical / evaluative question,
    // route it through the single-stage research-plan generator instead of
    // the outline → subtasks → executor pipeline. The research plan is a
    // one-shot deliverable; we tear down plan_mode after rendering so the
    // user falls back into normal chat to continue the discussion.
    if matches!(
        astra_runtime::plan_decompose::classify_plan_suggestion(&goal).map(|s| s.kind),
        Some(astra_runtime::plan_decompose::PlanKind::Analytical)
    ) {
        return handle_analytical_goal(goal, tok, state, api).await;
    }

    // ── Stage 1: Generate outline ───────────────────────────────────────
    eprintln!();
    let outline_prompt = outline::outline_prompt(&goal, &plan_state.context);
    let outline_payload = serde_json::json!({
        "messages": [{"role": "user", "content": outline_prompt}],
    });

    let outline_result = plan_llm_call(api, tok, &outline_payload).await;
    maybe_init_session_from_plan(state, &outline_result);
    let outline_text = match outline_result {
        PlanLlmOutcome::Ok { text, .. } => text,
        PlanLlmOutcome::Cancelled => {
            eprintln!("  {} Plan generation cancelled.", theme::icon_warn());
            abort_plan_mode_after_failure(state, "outline", "cancelled");
            return Ok(PlanInputResult::Handled);
        }
        PlanLlmOutcome::Error(e) => {
            eprintln!("  {} {}", theme::icon_err(), e.clone().red());
            abort_plan_mode_after_failure(state, "outline", &e);
            return Ok(PlanInputResult::Handled);
        }
    };

    if outline_text.trim().is_empty() {
        // LLM returned only thinking content, no text_delta — skip to full plan
        if state.verbose_mode {
            eprintln!("  {} Outline response empty, trying full plan…", "⋯".dim());
        }
    } else {
        // Try to parse as outline; fall back to full plan generation on failure
        let parsed_outline = outline::parse_outline_response(&outline_text);

        if state.verbose_mode {
            if let Err(ref e) = parsed_outline {
                eprintln!("  {} Outline parse error: {}", "⋯".dim(), e.as_str().dim());
                let preview: String = outline_text.chars().take(200).collect();
                eprintln!("  {} Response preview: {}", "⋯".dim(), preview.dim());
            }
        }

        match parsed_outline {
            Ok(ref ol) if !ol.questions.is_empty() => {
                return handle_outline_clarifications(
                    ol.questions.clone(),
                    &goal,
                    token,
                    state,
                    api,
                )
                .await;
            }
            Ok(ref ol) if !ol.phases.is_empty() => {
                eprintln!();
                eprint_styled_outline(ol, &goal);

                match prompt_outline_confirmation(ol.phases.len()) {
                    OutlineChoice::Confirm => {
                        return expand_outline_to_plan(ol, &goal, tok, state, api).await;
                    }
                    OutlineChoice::SkipToFull => {
                        eprintln!("  {} Generating full plan directly…", "⏭".cyan());
                    }
                    OutlineChoice::Edit => {
                        match inquire::Text::new("  Describe changes:")
                            .with_help_message("Esc to cancel")
                            .prompt()
                        {
                            Ok(edit) if !edit.trim().is_empty() => {
                                return Box::pin(handle_plan_mode_input(edit, token, state, api))
                                    .await;
                            }
                            _ => {}
                        }
                        return Ok(PlanInputResult::Handled);
                    }
                    OutlineChoice::Cancel => {
                        return Ok(PlanInputResult::Handled);
                    }
                }
            }
            _ => {
                // Outline parse failed — try as full plan or clarification directly
                if let Some(questions) = detect_clarification_questions(&outline_text) {
                    return handle_outline_clarifications(questions, &goal, token, state, api)
                        .await;
                }
                if let Ok(plan) = parse_plan_response(&outline_text) {
                    return accept_generated_plan(plan, token, state, api).await;
                }
                if state.verbose_mode {
                    eprintln!("  {} Outline parse failed, trying full plan…", "⋯".dim());
                }
            }
        }
    }

    // ── Fallback: direct full-plan generation ───────────────────────────
    let Some(plan_ctx) = state.plan_mode.as_ref().map(|ps| ps.context.clone()) else {
        return Ok(PlanInputResult::Handled);
    };
    let gen_result =
        plan_generate_with_retry(api, tok, &goal, &plan_ctx, state.session_id.as_deref()).await;
    maybe_init_session_from_plan(state, &gen_result);
    let full_text = match gen_result {
        PlanLlmOutcome::Ok { text, .. } => text,
        PlanLlmOutcome::Cancelled => {
            eprintln!("  {} Plan generation cancelled.", theme::icon_warn());
            abort_plan_mode_after_failure(state, "full_plan", "cancelled");
            return Ok(PlanInputResult::Handled);
        }
        PlanLlmOutcome::Error(e) => {
            eprintln!("  {} {}", theme::icon_err(), e.clone().red());
            abort_plan_mode_after_failure(state, "full_plan", &e);
            return Ok(PlanInputResult::Handled);
        }
    };

    if let Some(questions) = detect_clarification_questions(&full_text) {
        return handle_outline_clarifications(questions, &goal, token, state, api).await;
    }

    match parse_plan_response(&full_text) {
        Ok(plan) => accept_generated_plan(plan, token, state, api).await,
        Err(e) => {
            eprint_plan_json_parse_failed(&full_text, &e.to_string());
            abort_plan_mode_after_failure(state, "full_plan_parse", &e.to_string());
            Ok(PlanInputResult::Handled)
        }
    }
}

/// User's choice after seeing the outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutlineChoice {
    Confirm,
    SkipToFull,
    Edit,
    Cancel,
}

/// Interactive outline confirmation using `inquire::Select`.
fn prompt_outline_confirmation(phase_count: usize) -> OutlineChoice {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return OutlineChoice::Confirm;
    }

    let options = vec![
        format!("✓  Looks good, expand all {phase_count} phases"),
        "⏭  Skip outline, generate full plan directly".to_string(),
        "✏  Edit (describe changes)".to_string(),
        "✕  Cancel".to_string(),
    ];

    eprintln!();
    match inquire::Select::new("Plan outline:", options)
        .with_render_config(plan_select_theme())
        .without_help_message()
        .prompt()
    {
        Ok(c) if c.starts_with('✓') => OutlineChoice::Confirm,
        Ok(c) if c.starts_with('⏭') => OutlineChoice::SkipToFull,
        Ok(c) if c.starts_with('✏') => OutlineChoice::Edit,
        _ => OutlineChoice::Cancel,
    }
}

/// Handle clarification questions with interactive `inquire::Select`.
async fn handle_outline_clarifications(
    questions: Vec<plan::ClarificationQuestion>,
    goal: &str,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use plan::{PendingClarifications, PlanModeState};
    use std::io::IsTerminal;

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

    // Try interactive inquire selection if terminal supports it
    if std::io::stdin().is_terminal() {
        let mut answers = Vec::new();
        for q in &questions {
            match ask_clarification_interactive(q) {
                Some(answer) => answers.push(answer),
                None => {
                    // User cancelled — fall back to text-based flow
                    break;
                }
            }
        }

        if answers.len() == questions.len() {
            // All answered — regenerate plan with answers
            let answers_text = questions
                .iter()
                .zip(&answers)
                .map(|(q, a)| format!("Q: {}\nA: {}", q.question, a))
                .collect::<Vec<_>>()
                .join("\n\n");

            let goal_with_context =
                format!("{goal}\n\n## Clarifications from user:\n{answers_text}");

            let Some(tok) = token else {
                return Ok(PlanInputResult::Handled);
            };

            let Some(clarify_ctx) = state.plan_mode.as_ref().map(|ps| ps.context.clone()) else {
                return Ok(PlanInputResult::Handled);
            };
            let clarify_result = plan_generate_with_retry(
                api,
                tok,
                &goal_with_context,
                &clarify_ctx,
                state.session_id.as_deref(),
            )
            .await;
            maybe_init_session_from_plan(state, &clarify_result);
            let full_text = match clarify_result {
                PlanLlmOutcome::Ok { text, .. } => text,
                PlanLlmOutcome::Cancelled => {
                    eprintln!("  {} Plan generation cancelled.", theme::icon_warn());
                    return Ok(PlanInputResult::Handled);
                }
                PlanLlmOutcome::Error(e) => {
                    eprintln!("  {} {}", theme::icon_err(), e.red());
                    return Ok(PlanInputResult::Handled);
                }
            };

            match plan::parse_plan_response(&full_text) {
                Ok(plan) => return accept_generated_plan(plan, token, state, api).await,
                Err(e) => {
                    eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    return Ok(PlanInputResult::Handled);
                }
            }
        }
    }

    // Fall back to text-based clarification (existing flow)
    let Some(plan_state) = state.plan_mode.as_mut() else {
        return Ok(PlanInputResult::Handled);
    };
    let pending = PendingClarifications {
        questions: questions.clone(),
        answers: Vec::new(),
    };
    plan_state.pending_clarifications = Some(pending);
    eprint_clarification_question(&questions[0]);
    let _ = plan_state.save_to_file(&PlanModeState::state_path());
    Ok(PlanInputResult::Handled)
}

/// Ask a single clarification question using `inquire::Select`.
///
/// Returns the selected answer text, or `None` if the user pressed Esc.
fn ask_clarification_interactive(q: &plan::ClarificationQuestion) -> Option<String> {
    let icon = match q.category {
        plan::ClarificationCategory::Scope => "📦",
        plan::ClarificationCategory::Approach => "🛤️ ",
        plan::ClarificationCategory::Behavior => "⚙️ ",
        plan::ClarificationCategory::Technical => "🔧",
        plan::ClarificationCategory::Confirmation => "❓",
        plan::ClarificationCategory::Other => "💬",
    };

    let mut options = q.options.clone();
    options.push("Other (type your answer)".into());

    let prompt_text = format!("{icon} {}", q.question);
    let starting = q.default.unwrap_or(0);

    match inquire::Select::new(&prompt_text, options.clone())
        .with_render_config(plan_select_theme())
        .with_starting_cursor(starting)
        .without_help_message()
        .prompt()
    {
        Ok(choice) if choice.starts_with("Other") => {
            match inquire::Text::new("  Your answer:").prompt() {
                Ok(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
                _ => None,
            }
        }
        Ok(choice) => Some(choice),
        Err(_) => None,
    }
}

/// Expand an outline into a full plan by generating subtasks for each phase.
async fn expand_outline_to_plan(
    ol: &astra_runtime::plan::outline::PlanOutline,
    goal: &str,
    tok: &str,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use astra_runtime::plan::outline;
    use plan::parse_plan_response;

    let Some(plan_ctx) = state.plan_mode.as_ref().map(|ps| ps.context.clone()) else {
        return Ok(PlanInputResult::Handled);
    };
    let mut all_subtasks = Vec::new();
    let mut completed_phases = Vec::new();

    for (i, phase) in ol.phases.iter().enumerate() {
        eprintln!(
            "  {} Expanding phase {}/{}: {}",
            "⋯".cyan(),
            i + 1,
            ol.phases.len(),
            phase.title.as_str().bold()
        );

        let detail_prompt =
            outline::phase_detail_prompt(goal, ol, phase, &completed_phases, &plan_ctx);
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": detail_prompt}],
        });

        let expand_result = plan_llm_call(api, tok, &payload).await;
        maybe_init_session_from_plan(state, &expand_result);
        let text = match expand_result {
            PlanLlmOutcome::Ok { text: t, .. } => t,
            PlanLlmOutcome::Cancelled => {
                eprintln!("  {} Cancelled during phase expansion.", theme::icon_warn());
                // Use whatever subtasks we have so far
                if !all_subtasks.is_empty() {
                    eprintln!(
                        "  {} Using {} subtasks from completed phases.",
                        theme::icon_ok(),
                        all_subtasks.len()
                    );
                    break;
                }
                return Ok(PlanInputResult::Handled);
            }
            PlanLlmOutcome::Error(e) => {
                eprintln!(
                    "  {} Phase {} failed: {}",
                    theme::icon_warn(),
                    phase.id,
                    e.yellow()
                );
                continue;
            }
        };

        match parse_plan_response(&text) {
            Ok(phase_plan) => {
                let count = phase_plan.subtasks.len();
                all_subtasks.extend(phase_plan.subtasks);
                completed_phases.push(phase.id.clone());
                eprintln!(
                    "  {} {} — {} subtask{}",
                    theme::icon_ok(),
                    phase.title,
                    count,
                    if count == 1 { "" } else { "s" }
                );
            }
            Err(_) => {
                // Retry once with a stricter prompt that includes the full schema
                let retry_prompt = format!(
                    "Your previous response was not valid JSON. Output ONLY this JSON object, \
                     no markdown fences, no explanation:\n\
                     {{\"subtasks\": [{{\"id\": \"{}-step-1\", \"title\": \"...\", \
                     \"description\": \"...\", \"depends_on\": [], \"effort\": \"small\", \
                     \"files\": [], \"acceptance_checks\": [{{\"kind\": \"file_exists\", \
                     \"paths\": [\"tmp/x\"]}}]}}]}}\n\
                     Expand phase \"{}\" — {}",
                    phase.id, phase.id, phase.title
                );
                let retry_payload = serde_json::json!({
                    "messages": [
                        {"role": "user", "content": detail_prompt},
                        {"role": "assistant", "content": text},
                        {"role": "user", "content": retry_prompt},
                    ],
                });
                let retry_result = plan_llm_call(api, tok, &retry_payload).await;
                maybe_init_session_from_plan(state, &retry_result);
                match retry_result {
                    PlanLlmOutcome::Ok {
                        text: retry_text, ..
                    } => match parse_plan_response(&retry_text) {
                        Ok(phase_plan) => {
                            let count = phase_plan.subtasks.len();
                            all_subtasks.extend(phase_plan.subtasks);
                            completed_phases.push(phase.id.clone());
                            eprintln!(
                                "  {} {} — {} subtask{} (retry)",
                                theme::icon_ok(),
                                phase.title,
                                count,
                                if count == 1 { "" } else { "s" }
                            );
                        }
                        Err(e2) => {
                            eprintln!(
                                "  {} Phase {} failed: {}",
                                theme::icon_warn(),
                                phase.id,
                                e2.yellow()
                            );
                        }
                    },
                    PlanLlmOutcome::Cancelled => break,
                    PlanLlmOutcome::Error(e) => {
                        eprintln!(
                            "  {} Phase {} retry failed: {}",
                            theme::icon_warn(),
                            phase.id,
                            e.yellow()
                        );
                    }
                }
            }
        }
    }

    if all_subtasks.is_empty() {
        eprintln!(
            "  {} No subtasks generated. Try rephrasing your goal.",
            theme::icon_err()
        );
        return Ok(PlanInputResult::Handled);
    }

    let plan = astra_services::task_orchestrator::TaskPlan {
        subtasks: all_subtasks,
        notes: Some(format!("Generated from {}-phase outline", ol.phases.len())),
    };

    accept_generated_plan(plan, Some(tok), state, api).await
}

/// Accept a generated plan: store it, journal it, record a session turn, and prompt for execution.
async fn accept_generated_plan(
    plan: astra_services::task_orchestrator::TaskPlan,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<PlanInputResult, String> {
    use plan::PlanModeState;

    let Some(plan_state) = state.plan_mode.as_mut() else {
        return Ok(PlanInputResult::Handled);
    };
    plan_state.set_plan(plan);
    let _ = plan_state.save_to_file(&PlanModeState::state_path());

    let subtask_count = plan_state.plan.subtasks.len();
    let goal = plan_state.goal.clone();

    journal_plan_event(
        &mut state.journal,
        session_journal::JournalEventType::PlanLifecycle,
        &format!("Plan generated: {subtask_count} subtasks"),
        Some(serde_json::json!({
            "subtask_count": subtask_count,
        })),
    );

    // Record a session turn so /session shows the plan generation
    if let Some(ref journal) = state.journal {
        let plan_summary: Vec<String> = plan_state
            .plan
            .subtasks
            .iter()
            .map(|s| format!("- [{}] {}", s.id, s.title))
            .collect();
        let assistant_content = format!(
            "Plan generated ({subtask_count} subtasks):\n{}",
            plan_summary.join("\n")
        );
        let turn_event = session_journal::JournalEvent::turn(
            state.session_id.as_deref(),
            state.turn,
            state.model.as_deref(),
            &goal,
            &assistant_content,
            0, // tool_count
            0, // tokens_in (not tracked for plan generation)
            0, // tokens_out
            0, // duration_ms
        );
        let _ = journal.append(&turn_event);
    }
    state.turn += 1;

    // Show the generated plan before asking what to do
    eprintln!();
    eprint_styled_plan(&plan_state.plan, &goal);

    if let Some(choice) = prompt_plan_confirmation(subtask_count) {
        match choice {
            PlanConfirmChoice::ExecuteAll => {
                return Box::pin(handle_plan_mode_input("go".into(), token, state, api)).await;
            }
            PlanConfirmChoice::StepByStep => {
                return Box::pin(handle_plan_mode_input("step".into(), token, state, api)).await;
            }
            PlanConfirmChoice::Edit => {
                match inquire::Text::new("  Describe changes:")
                    .with_help_message("Esc to cancel")
                    .prompt()
                {
                    Ok(edit) if !edit.trim().is_empty() => {
                        return Box::pin(handle_plan_mode_input(edit, token, state, api)).await;
                    }
                    _ => {}
                }
            }
            PlanConfirmChoice::Cancel => {}
        }
    }

    Ok(PlanInputResult::Handled)
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
        assert!(!try_replace_plan_from_llm_json("Just use natural language.", &mut ps).unwrap());
    }

    #[test]
    fn try_replace_plan_from_llm_json_bad_json_returns_err() {
        let mut ps = plan::PlanModeState::new("g".into(), plan::ProjectContext::default());
        // Has `{` but invalid JSON → real parse error
        let err = try_replace_plan_from_llm_json("Here is the plan: {broken", &mut ps).unwrap_err();
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

    #[tokio::test]
    async fn paused_plan_slash_command_is_redispatched() {
        let mut state = ReplState::default();
        state.plan_mode = Some(plan::PlanModeState::new(
            "goal".into(),
            plan::ProjectContext::default(),
        ));
        let (handle, _update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();

        let result = handle_plan_mode_input("/help".into(), None, &mut state, &api)
            .await
            .unwrap();

        match result {
            PlanInputResult::DispatchSlash(cmd) => assert_eq!(cmd, "/help"),
            other => panic!("expected DispatchSlash, got {other:?}"),
        }
        assert!(
            state.plan_mode.is_some(),
            "slash command should not abandon the plan"
        );
        assert!(
            state.plan_handle.is_some(),
            "slash command should leave the paused executor intact"
        );
    }

    #[tokio::test]
    async fn paused_plan_plain_text_abandons_and_sends_chat() {
        let mut state = ReplState::default();
        state.plan_mode = Some(plan::PlanModeState::new(
            "goal".into(),
            plan::ProjectContext::default(),
        ));
        let (handle, _update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();

        let result = handle_plan_mode_input("tell me more".into(), None, &mut state, &api)
            .await
            .unwrap();

        match result {
            PlanInputResult::SendAsChat(msg) => assert_eq!(msg, "tell me more"),
            other => panic!("expected SendAsChat, got {other:?}"),
        }
        assert!(
            state.plan_mode.is_none(),
            "plain chat should abandon paused plan mode"
        );
        assert!(
            state.plan_handle.is_none(),
            "plain chat should clear the paused executor handle"
        );
    }

    #[tokio::test]
    async fn execute_exits_plan_mode_and_leaves_background_state() {
        let mut state = ReplState::default();
        state.chat_plan_only = true;
        let mut ps = plan::PlanModeState::new("goal".into(), plan::ProjectContext::default());
        ps.plan.subtasks.push(SubtaskPlan {
            id: "s1".into(),
            title: "one".into(),
            ..Default::default()
        });
        state.plan_mode = Some(ps);

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();

        let result = handle_plan_mode_input("go".into(), None, &mut state, &api)
            .await
            .unwrap();

        assert!(matches!(result, PlanInputResult::Handled));
        assert!(state.plan_mode.is_none(), "execute should leave plan mode");
        assert!(
            state.executing_plan.is_some(),
            "execute should preserve plan for background status"
        );
        assert_eq!(state.executing_plan_goal.as_deref(), Some("goal"));
        assert!(
            !state.chat_plan_only,
            "execute should restore normal chat after leaving plan mode"
        );
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

    #[test]
    fn recover_plan_for_resume_resets_failed_to_pending() {
        use astra_services::task_orchestrator::TaskPlan;
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "done".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "failed".into(),
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s3".into(),
                    title: "pending".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let result = super::recover_plan_for_resume(&mut plan);
        match result {
            super::PlanResumeRecovery::Ready(recovered) => {
                assert_eq!(recovered.subtasks[0].status, TaskStatus::Completed);
                assert_eq!(recovered.subtasks[1].status, TaskStatus::Pending);
                assert_eq!(recovered.subtasks[2].status, TaskStatus::Pending);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn recover_plan_for_resume_all_completed_returns_all_completed() {
        use astra_services::task_orchestrator::TaskPlan;
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "done".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "also done".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert!(matches!(
            super::recover_plan_for_resume(&mut plan),
            super::PlanResumeRecovery::AllCompleted
        ));
    }

    #[test]
    fn recover_plan_for_resume_empty_subtasks_returns_empty_no_subtasks() {
        // B4: An empty subtasks list means generation never produced a plan.
        // Previously this fell into the "AllCompleted" branch and confused the
        // user with "All subtasks already completed — nothing to resume".
        use astra_services::task_orchestrator::TaskPlan;
        let mut plan = TaskPlan {
            subtasks: vec![],
            notes: None,
        };
        assert!(matches!(
            super::recover_plan_for_resume(&mut plan),
            super::PlanResumeRecovery::EmptyNoSubtasks
        ));
    }

    #[test]
    fn recover_plan_for_resume_preserves_completed_subtasks() {
        use astra_services::task_orchestrator::TaskPlan;
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "done".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "failed".into(),
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        if let super::PlanResumeRecovery::Ready(recovered) =
            super::recover_plan_for_resume(&mut plan)
        {
            // Completed subtask untouched
            assert_eq!(recovered.subtasks[0].status, TaskStatus::Completed);
            // ready_subtasks() should now return s2 (Pending, no deps)
            let ready = recovered.ready_subtasks();
            assert_eq!(ready.len(), 1);
            assert_eq!(ready[0].id, "s2");
        } else {
            panic!("expected Ready");
        }
    }

    // ── Plan session_id fixes ────────────────────────────────────────────

    #[test]
    fn maybe_init_session_from_plan_adopts_server_session_even_when_local_exists() {
        let mut state = ReplState::default();
        // Simulate local session_id generated at plan entry
        state.session_id = Some("local-uuid-1234".to_string());

        let outcome = PlanLlmOutcome::Ok {
            text: "plan text".to_string(),
            session_id: Some("server-session-5678".to_string()),
        };
        maybe_init_session_from_plan(&mut state, &outcome);

        assert_eq!(
            state.session_id.as_deref(),
            Some("server-session-5678"),
            "should adopt server session_id even when local one exists"
        );
    }

    #[test]
    fn maybe_init_session_from_plan_noop_when_already_matching() {
        let mut state = ReplState::default();
        state.session_id = Some("same-id".to_string());

        let outcome = PlanLlmOutcome::Ok {
            text: "text".to_string(),
            session_id: Some("same-id".to_string()),
        };
        maybe_init_session_from_plan(&mut state, &outcome);

        assert_eq!(state.session_id.as_deref(), Some("same-id"));
    }

    #[test]
    fn maybe_init_session_from_plan_noop_on_no_server_session() {
        let mut state = ReplState::default();
        state.session_id = Some("local-id".to_string());

        let outcome = PlanLlmOutcome::Ok {
            text: "text".to_string(),
            session_id: None,
        };
        maybe_init_session_from_plan(&mut state, &outcome);

        assert_eq!(
            state.session_id.as_deref(),
            Some("local-id"),
            "should keep local id when server returns no session"
        );
    }
}
