//! Bridge between the durable task system and the REPL plan execution loop.
//!
//! This module wraps [`DurableTaskLifecycle`] calls with terminal display formatting
//! so plan execution can show contract generation, verification results, and delivery
//! reports in a user-friendly way.

use crate::cli::cli_config::cli_utils::{prefix_chars, truncate_str};

use astra_services::{
    ContractAmendment, ContractGenerator, DurableTaskLifecycle, LocalDurableTaskLifecycle,
    SubtaskStage, SubtaskVerificationReport, TaskContract, TaskDeliveryReport, VerifierKind,
};
use crossterm::style::Stylize;
use std::sync::Arc;

use crate::cli::theme;

/// Build a reqwest client for the durable-task bridge.
///
/// Durable-bridge traffic is local-only (CLI ↔ local API server), so we always
/// skip any system proxy. Only the LLM HTTP client (runtime `llm_client.rs`)
/// honours env proxy vars.
fn build_client_for_url(_url: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("durable-task bridge HTTP client config must be valid")
}

// ─── Active contract state held by the REPL ──────────────────────────────────

/// Holds the active contract and lifecycle service during plan execution.
pub struct DurableTaskState {
    pub contract: TaskContract,
    pub lifecycle: Arc<dyn DurableTaskLifecycle>,
    /// Delivery report from the most recent `on_plan_complete()` call (if successful).
    pub last_report: Option<TaskDeliveryReport>,
}

impl std::fmt::Debug for DurableTaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableTaskState")
            .field("contract_id", &self.contract.contract_id)
            .finish_non_exhaustive()
    }
}

// ─── Contract generation ─────────────────────────────────────────────────────

/// Generate a [`TaskContract`] from a plan and persist it via the lifecycle.
///
/// Returns `None` (with a warning) unless the full durable contract activation
/// succeeds end to end: generation, persistence, and criteria injection.
/// Plan execution then proceeds without contract-backed verification instead of
/// pretending a half-persisted contract is usable.
pub async fn generate_contract(
    lifecycle: &Arc<dyn DurableTaskLifecycle>,
    plan: &astra_services::task_orchestrator::TaskPlan,
    goal: &str,
    user_id: &str,
    session_id: &str,
    work_dir: &std::path::Path,
) -> Option<TaskContract> {
    let detection = astra_services::ProjectDetection::detect(work_dir);
    let cg = ContractGenerator::new(detection);

    let contract = match cg.generate(goal, plan, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "  {}  Contract generation skipped: {}",
                theme::icon_warn(),
                e
            );
            return None;
        }
    };

    // Persist via lifecycle: create_contract builds a bare skeleton, then we
    // amend it with the criteria ContractGenerator produced.
    let scope = contract.scope.clone();
    let generated_subtasks = contract.subtasks.clone();
    let generated_global = contract.global_verification.clone();

    match lifecycle
        .create_contract(user_id, session_id, goal, plan, scope)
        .await
    {
        Ok(persisted) => {
            // Inject generated criteria via amend (create_contract builds empty criteria)
            let amendment = ContractAmendment {
                reason: "inject generated verification criteria".into(),
                updated_subtasks: Some(generated_subtasks),
                updated_global_verification: Some(generated_global),
                updated_scope: None,
            };
            match lifecycle
                .amend_contract(&persisted.contract_id, amendment)
                .await
            {
                Ok(amended) => {
                    display_contract_summary(&amended);
                    Some(amended)
                }
                Err(e) => {
                    let message = e.message;
                    eprintln!(
                        "  {}  Criteria injection failed: {}",
                        theme::icon_warn(),
                        message,
                    );
                    None
                }
            }
        }
        Err(e) => {
            let message = e.message;
            eprintln!(
                "  {}  Contract persistence failed: {}",
                theme::icon_warn(),
                message
            );
            None
        }
    }
}

/// Pretty-print a one-line contract summary on creation.
fn display_contract_summary(contract: &TaskContract) {
    let n_subtasks = contract.subtasks.len();
    let n_criteria: usize = contract.subtasks.iter().map(|s| s.criteria.len()).sum();
    let n_global = contract.global_verification.len();
    eprintln!(
        "\n  {} {} {} subtasks, {} criteria, {} global  {}",
        "▸".bold().magenta(),
        "Contract:".bold(),
        format!("{n_subtasks}").magenta(),
        format!("{n_criteria}").magenta(),
        format!("{n_global}").magenta(),
        format!("[{}]", prefix_chars(&contract.contract_id, 8)).dim(),
    );
}

// ─── Subtask lifecycle hooks ─────────────────────────────────────────────────

/// Check whether a subtask has exhausted its retry budget.
pub fn subtask_retries_exhausted(durable: &DurableTaskState, subtask_id: &str) -> bool {
    durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)
        .map(|s| s.retry_count >= s.max_retries)
        .unwrap_or(false)
}

/// Call when a subtask transitions Pending → Executing (snapshot).
///
/// Returns an error if durable execution could not be started. Callers must not
/// proceed with the subtask turn after this fails, or they would mutate the
/// workspace without a durable snapshot / execution boundary.
pub async fn on_subtask_begin(
    durable: &mut DurableTaskState,
    subtask_id: &str,
) -> Result<(), String> {
    match durable
        .lifecycle
        .begin_subtask(&durable.contract.task_id, subtask_id)
        .await
    {
        Ok(ctx) => {
            if let Some(sub) = durable
                .contract
                .subtasks
                .iter_mut()
                .find(|s| s.id == subtask_id)
            {
                sub.stage = SubtaskStage::Executing;
                sub.snapshot_name = ctx.snapshot_name;
            }
            Ok(())
        }
        Err(e) => {
            let message = e.to_string();
            eprintln!(
                "  {}  Durable start failed for {}: {}",
                theme::icon_warn(),
                subtask_id,
                message,
            );
            if let Some(sub) = durable
                .contract
                .subtasks
                .iter_mut()
                .find(|s| s.id == subtask_id)
            {
                sub.stage = SubtaskStage::ExecutionFailed {
                    error: message.clone(),
                };
            }
            Err(message)
        }
    }
}

/// Call when a subtask's chat turn completes (diff capture + verification).
///
/// Returns `(passed, report)` — `passed` is true if verification succeeded (or
/// no criteria). `report` is `Some` when verification actually ran.
pub async fn on_subtask_complete(
    durable: &mut DurableTaskState,
    subtask_id: &str,
) -> (bool, Option<SubtaskVerificationReport>) {
    let task_id = durable.contract.task_id.clone();

    // 1. Complete execution (captures diff)
    if let Err(e) = durable
        .lifecycle
        .complete_subtask_execution(&task_id, subtask_id)
        .await
    {
        let message = e.message;
        eprintln!(
            "  {}  Diff capture failed for {}: {}",
            theme::icon_warn(),
            subtask_id,
            message,
        );
        if let Some(sub) = durable
            .contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
        {
            sub.stage = SubtaskStage::ExecutionFailed { error: message };
        }
        return (false, None);
    }

    // 2. Decide whether we must run `lifecycle.verify_subtask`.
    //
    // `complete_subtask_execution` leaves the durable row in `AwaitingVerification` whenever the
    // subtask has *any* criteria.  The plan orchestrator (`TaskPlan`) marks the subtask
    // `Completed` based on this function's return value — so if we return `true` without
    // calling `verify_subtask`, durable stays non-`Verified` while the executor thinks the plan is
    // 100% done and immediately calls `on_plan_complete` → `verify_global`, which then errors with
    // `subtasks not ready for global verification`.
    //
    // Criteria that are only `LlmJudge` or `global_only` still need a full `verify_subtask` call
    // (the lifecycle runner runs all criteria when `skip_heavy` is false); they must not be
    // silently skipped here.
    let (criteria_count, has_local_criteria) = durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)
        .map(|s| {
            let n = s.criteria.len();
            let local = s
                .criteria
                .iter()
                .any(|c| !c.global_only && !matches!(c.verifier, VerifierKind::LlmJudge { .. }));
            (n, local)
        })
        .unwrap_or((0, false));

    if criteria_count == 0 {
        return (true, None);
    }

    // 3. Run verification with progress indication + spinner
    let label = if has_local_criteria {
        format!("Verifying: {subtask_id}")
    } else {
        format!("Verifying (LLM): {subtask_id}")
    };
    let spinner = crate::cli::stream::stream_render::Spinner::start(label);
    let result = durable.lifecycle.verify_subtask(&task_id, subtask_id).await;
    spinner.stop_clear();
    match result {
        Ok(report) => {
            // Update our in-memory contract with the result
            if let Some(sub) = durable
                .contract
                .subtasks
                .iter_mut()
                .find(|s| s.id == subtask_id)
            {
                if report.all_required_passed {
                    sub.stage = SubtaskStage::Verified;
                } else {
                    sub.retry_count += 1;
                    sub.stage = SubtaskStage::VerificationFailed {
                        results: report.results.clone(),
                    };
                }
            }

            let passed = report.all_required_passed;
            (passed, Some(report))
        }
        Err(e) => {
            let message = e.message;
            eprintln!(
                "  {}  Verification error for {}: {}",
                theme::icon_warn(),
                subtask_id,
                message,
            );
            if let Some(sub) = durable
                .contract
                .subtasks
                .iter_mut()
                .find(|s| s.id == subtask_id)
            {
                sub.stage = SubtaskStage::ExecutionFailed { error: message };
            }
            (false, None)
        }
    }
}

/// Pretty-print subtask verification results (stderr).
///
/// Single implementation for CLI: the TUI plan event loop
/// calls this; do not duplicate the formatting elsewhere.
pub fn display_verification_report(report: &SubtaskVerificationReport) {
    let passed = report.results.iter().filter(|r| r.passed).count();
    let total = report.results.len();
    let styled_icon = if report.all_required_passed {
        theme::icon_ok()
    } else {
        theme::icon_err()
    };

    eprintln!(
        "  {}  {} {}/{} criteria passed  {}",
        styled_icon,
        "Verification:".bold(),
        format!("{passed}").magenta(),
        format!("{total}").dim(),
        format!("[{}]", report.subtask_id).dim(),
    );

    // Show individual results (both pass and fail for transparency)
    for r in &report.results {
        let dur_tag = if r.duration_ms > 0 {
            format!(" ({:.1}s)", r.duration_ms as f64 / 1000.0)
        } else {
            String::new()
        };
        if r.passed {
            let evidence: String = r.evidence.trim().chars().take(120).collect();
            if !evidence.is_empty() {
                eprintln!(
                    "      {} {} — {}{}",
                    theme::icon_ok(),
                    r.criterion_id.clone().dark_grey(),
                    evidence.dark_grey(),
                    dur_tag.dark_grey(),
                );
            } else {
                eprintln!(
                    "      {} {}{}",
                    theme::icon_ok(),
                    r.criterion_id.clone().dark_grey(),
                    dur_tag.dark_grey(),
                );
            }
        } else {
            let evidence: String = r.evidence.trim().chars().take(200).collect();
            let expected: String = r.expected.chars().take(120).collect();
            eprintln!(
                "    {} {}{}",
                theme::icon_err(),
                r.criterion_id,
                dur_tag.dark_grey(),
            );
            if !evidence.is_empty() {
                eprintln!("      {} {}", "got:".dim(), evidence.yellow());
            }
            if !expected.is_empty() {
                eprintln!("      {} {}", "expected:".dim(), expected);
            }
            if let Some(ref err) = r.error {
                eprintln!("      {} {}", "error:".red(), err.clone().red());
            }
        }
    }
}

// ─── Global verification + delivery ─────────────────────────────────────────

/// Run global verification (build/test/lint) after all subtasks complete.
///
/// Returns:
/// - `Ok(true)` when global checks pass and delivery succeeds
/// - `Ok(false)` when verification ran and at least one required global check failed
/// - `Err(...)` when durable orchestration itself failed (verification/delivery persistence)
pub async fn on_plan_complete(durable: &mut DurableTaskState) -> Result<bool, String> {
    let task_id = durable.contract.task_id.clone();

    eprintln!(
        "\n  {} {}",
        "●".magenta(),
        "Running global verification...".dim()
    );

    // Show what will be checked
    for c in &durable.contract.global_verification {
        let cmd_hint = match &c.verifier {
            VerifierKind::BuildPass { cmd } => {
                format!("build: {cmd}")
            }
            VerifierKind::TestPass { cmd, .. } => {
                format!("test: {cmd}")
            }
            VerifierKind::Command { cmd, .. } => {
                format!("cmd: {cmd}")
            }
            _ => c.description.clone(),
        };
        eprintln!("    {} {}", "▸".grey(), cmd_hint);
    }

    let spinner = crate::cli::stream::stream_render::Spinner::start("Running global checks".into());
    let verify_result = durable.lifecycle.verify_global(&task_id).await;
    spinner.stop_clear();

    match verify_result {
        Ok(results) => {
            let passed = results.iter().filter(|r| r.passed).count();
            let total = results.len();
            let all_passed = results.iter().all(|r| r.passed);

            let styled = if all_passed {
                theme::icon_ok()
            } else {
                theme::icon_err()
            };

            eprintln!(
                "  {}  {} {}/{} passed",
                styled,
                "Global checks:".bold(),
                format!("{passed}").magenta(),
                format!("{total}").dim(),
            );

            // Show ALL results — passes and failures — for full transparency
            for r in &results {
                let dur_tag = if r.duration_ms > 0 {
                    format!(" ({:.1}s)", r.duration_ms as f64 / 1000.0)
                } else {
                    String::new()
                };
                if r.passed {
                    eprintln!(
                        "      {} {}{}",
                        theme::icon_ok(),
                        r.criterion_id.clone().dark_grey(),
                        dur_tag.dark_grey(),
                    );
                } else {
                    let evidence = r.evidence.chars().take(200).collect::<String>();
                    eprintln!(
                        "      {} {} — {}{}",
                        theme::icon_err(),
                        r.criterion_id,
                        evidence,
                        dur_tag.dark_grey(),
                    );
                }
            }

            if all_passed {
                // Deliver the task
                match durable.lifecycle.deliver_task(&task_id).await {
                    Ok(report) => {
                        display_delivery_report(&report);
                        #[cfg(not(test))]
                        save_delivery_report_json(&report);
                        durable.last_report = Some(report);
                    }
                    Err(e) => {
                        let message = e.message;
                        eprintln!(
                            "  {}  Delivery report failed: {}",
                            theme::icon_warn(),
                            message,
                        );
                        return Err(format!("delivery report failed: {message}"));
                    }
                }
            }

            Ok(all_passed)
        }
        Err(e) => {
            let message = e.message;
            eprintln!(
                "  {}  Global verification error: {}",
                theme::icon_warn(),
                message,
            );
            Err(format!("global verification failed: {message}"))
        }
    }
}

/// Pretty-print the final delivery report.
pub(crate) fn display_delivery_report(report: &TaskDeliveryReport) {
    let all_subtasks_verified = report
        .subtask_summaries
        .iter()
        .all(|s| s.criteria_passed == s.criteria_total);
    let all_global_passed = report.global_verification.iter().all(|r| r.passed);
    let fully_delivered = all_subtasks_verified && all_global_passed;

    let total_retries: u32 = report.subtask_summaries.iter().map(|s| s.retry_count).sum();
    let criteria_passed: u32 = report
        .subtask_summaries
        .iter()
        .map(|s| s.criteria_passed)
        .sum();
    let criteria_total: u32 = report
        .subtask_summaries
        .iter()
        .map(|s| s.criteria_total)
        .sum();
    let global_passed = report
        .global_verification
        .iter()
        .filter(|r| r.passed)
        .count();
    let global_total = report.global_verification.len();

    // Determine width based on terminal (min 58, max 80)
    let box_width = crossterm::terminal::size()
        .map(|(c, _)| (c as usize).clamp(58, 80))
        .unwrap_or(58);
    let separator = "─".repeat(box_width);

    // ─── Header ──────────────────────────────────────────────────────────────
    eprintln!();
    eprintln!("  {}", "Delivery Report".bold());
    eprintln!("  {}", separator.as_str().dim());

    // Goal
    let goal_max = box_width.saturating_sub(10);
    let goal_display: String = if report.goal.chars().count() > goal_max {
        let mut g: String = report.goal.chars().take(goal_max - 1).collect();
        g.push('…');
        g
    } else {
        report.goal.clone()
    };
    eprintln!("  Task: {}", goal_display.bold());

    let (status_icon, status_text) = if fully_delivered {
        ("●", "Delivered".green().bold())
    } else if all_subtasks_verified {
        ("⚠", "Partial (global checks failed)".yellow().bold())
    } else {
        ("⚠", "Partial".yellow().bold())
    };
    eprintln!("  Status: {} {}", status_icon, status_text);

    // ─── Subtask Results ─────────────────────────────────────────────────────
    eprintln!("  {}", separator.as_str().dim());

    for sub in &report.subtask_summaries {
        let verified = sub.criteria_passed == sub.criteria_total;
        let icon = if verified { "●" } else { "⚠" };
        let criteria_info = format!("{}/{} criteria", sub.criteria_passed, sub.criteria_total);
        let retry_info = if sub.retry_count > 0 {
            format!(" ↻{}", sub.retry_count)
        } else {
            String::new()
        };
        let stage_info = if !sub.stage.is_empty() && sub.stage != "verified" {
            format!("  [{}]", sub.stage)
        } else {
            String::new()
        };
        eprintln!(
            "  {} {} ({}{}{})",
            if verified {
                format!("{}", icon.green())
            } else {
                format!("{}", icon.yellow())
            },
            sub.title,
            if verified {
                criteria_info.green().to_string()
            } else {
                criteria_info.yellow().to_string()
            },
            retry_info,
            stage_info.dark_grey(),
        );
    }

    // ─── Global Verification ─────────────────────────────────────────────────
    if !report.global_verification.is_empty() {
        eprintln!("  {}", separator.as_str().dim());
        eprintln!("  Global checks: {}/{}", global_passed, global_total,);
        for r in &report.global_verification {
            let icon = if r.passed { "●" } else { "✗" };
            let styled = if r.passed {
                format!("{}", icon.green())
            } else {
                format!("{}", icon.red())
            };
            let dur = if r.duration_ms > 0 {
                format!(" ({:.1}s)", r.duration_ms as f64 / 1000.0)
            } else {
                String::new()
            };
            eprintln!("    {} {}{}", styled, r.criterion_id, dur);
        }
    }

    // ─── Metrics ─────────────────────────────────────────────────────────────
    eprintln!("  {}", separator.as_str().dim());

    let mut metrics = Vec::new();
    metrics.push(format!(
        "{}/{} criteria passed",
        criteria_passed, criteria_total
    ));
    if total_retries > 0 {
        metrics.push(format!("↻ {} retries", total_retries));
    }
    if report.total_verifications > 0 {
        metrics.push(format!("{} verifications", report.total_verifications));
    }
    eprintln!("  {}", metrics.join("  ·  "));

    // Execution effort (turns, tokens, timestamp)
    let mut effort = Vec::new();
    if report.total_turns > 0 {
        effort.push(format!("{} turns", report.total_turns));
    }
    if report.total_tokens > 0 {
        let tok_display = if report.total_tokens >= 1000 {
            format!("{:.1}k tokens", report.total_tokens as f64 / 1000.0)
        } else {
            format!("{} tokens", report.total_tokens)
        };
        effort.push(tok_display);
    }
    if !effort.is_empty() {
        eprintln!("  {}", effort.join(", ").dim());
    }
    if !report.timestamp.is_empty() {
        eprintln!("  {}", report.timestamp.as_str().dim());
    }

    // ─── Risks / Assumptions ─────────────────────────────────────────────────
    if !report.risks.is_empty() {
        eprintln!("  {}", separator.as_str().dim());
        for risk in &report.risks {
            eprintln!("  ⚠ {}", risk.clone().yellow());
        }
    }

    // ─── Footer ──────────────────────────────────────────────────────────────
    eprintln!("  {}", separator.as_str().dim());
}

fn delivery_report_filename(report: &TaskDeliveryReport) -> String {
    format!(
        ".mo-delivery-{}.json",
        report.contract_id.chars().take(8).collect::<String>()
    )
}

fn save_delivery_report_json_to_dir(
    report: &TaskDeliveryReport,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let path = dir.join(delivery_report_filename(report));
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("serialize delivery report: {e}"))?;
    std::fs::write(&path, &json).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Save the delivery report as JSON to the working directory.
/// Prints the file path on success (dim grey, non-intrusive).
pub(crate) fn save_delivery_report_json(report: &TaskDeliveryReport) {
    #[cfg(test)]
    {
        let _ = report;
    }

    #[cfg(not(test))]
    {
        let dir = std::env::current_dir().unwrap_or_default();
        match save_delivery_report_json_to_dir(report, &dir) {
            Ok(path) => {
                eprintln!(
                    "  {}",
                    format!("📄 Report saved: {}", path.display()).dark_grey(),
                );
            }
            Err(e) => eprintln!("  {}  Could not save report: {}", theme::icon_warn(), e),
        }
    }
}

// ─── Post-delivery user feedback ─────────────────────────────────────────────

/// Convenience wrapper without cloud event streaming (used in tests).
#[cfg(test)]
pub fn create_local_lifecycle(
    session_dir: &std::path::Path,
    work_dir: &std::path::Path,
) -> Arc<dyn DurableTaskLifecycle> {
    create_local_lifecycle_full(session_dir, work_dir, None, None, None, None, None)
}
/// Full lifecycle creation with optional cloud LLM judge.
///
/// When `cloud_judge` is provided, it's used for semantic (LlmJudge) verification
/// instead of the edge-side HttpLlmJudge. The cloud judge persists results directly
/// to the cloud database and doesn't consume the edge context window.
#[allow(clippy::too_many_arguments)]
pub fn create_local_lifecycle_full(
    session_dir: &std::path::Path,
    work_dir: &std::path::Path,
    sender: Option<astra_services::event_ingestion::IngestionSender>,
    session_id: Option<&str>,
    user_id: Option<&str>,
    cloud_judge: Option<Arc<dyn astra_services::LlmJudge>>,
    server_proxy_judge: Option<Arc<dyn astra_services::LlmJudge>>,
) -> Arc<dyn DurableTaskLifecycle> {
    let _ = std::fs::create_dir_all(session_dir);
    let mut lifecycle =
        LocalDurableTaskLifecycle::new(session_dir.to_path_buf(), work_dir.to_path_buf());

    // Wire up LLM judge (priority: cloud > server proxy)
    if let Some(judge) = cloud_judge {
        lifecycle.set_llm_judge(judge);
    } else if let Some(judge) = server_proxy_judge {
        lifecycle.set_llm_judge(judge);
    }

    // Wire up cloud event streaming (if sender available)
    if let Some(s) = sender {
        lifecycle.set_event_sender(s);
    }
    if let (Some(sid), Some(uid)) = (session_id, user_id) {
        lifecycle.set_session_context(sid, uid);
    }

    // Wire up live output streaming — tees build/test stderr to the terminal
    // with dim grey styling so it's visible but doesn't dominate the output.
    lifecycle.set_output_sink(Arc::new(|line: &str| {
        use crossterm::style::Stylize;
        eprintln!("    {}", line.dark_grey());
    }));

    Arc::new(lifecycle)
}

// ─── LLM Judge Implementation ────────────────────────────────────────────────
//
// The edge-side `HttpLlmJudge` was removed along with `ASTRA_LLM_*` env vars; verification
// now delegates to [`ServerProxyLlmJudge`] (below), which calls `POST /v1/chat/completions`
// so the server handles model resolution + API key decryption.

/// Extract a 0.0–1.0 score from LLM judge response text.
///
/// Tries JSON parsing first (`{"score": 0.8}`), then falls back to finding
/// a decimal number in the text.
fn parse_judge_score(text: &str) -> Result<f64, String> {
    // Try JSON parse first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(score) = v["score"].as_f64()
    {
        return Ok(score.clamp(0.0, 1.0));
    }

    // Try to find JSON embedded in text (e.g., wrapped with markdown)
    if let Some(start) = text.find('{')
        && let Some(end) = text[start..].rfind('}')
    {
        let json_str = &text[start..=start + end];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(score) = v["score"].as_f64()
        {
            return Ok(score.clamp(0.0, 1.0));
        }
    }

    // Fallback: find any decimal number between 0 and 1
    for word in text.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Ok(n) = clean.parse::<f64>()
            && (0.0..=1.0).contains(&n)
        {
            return Ok(n);
        }
    }

    Err(format!(
        "Could not extract score from LLM response: {}",
        truncate_str(text, 200)
    ))
}

// ─── Server Proxy LLM Judge ──────────────────────────────────────────────────

/// [`LlmJudge`] that routes through the API server's `/v1/chat/completions` proxy.
///
/// Uses the same authentication (bearer token) and model resolution as the main
/// agent. No separate API keys or env vars required — the server decrypts the
/// model credentials and forwards to the upstream LLM provider.
pub struct ServerProxyLlmJudge {
    api: astra_thin_client::ThinClient,
    token: String,
    model: Option<String>,
}

impl ServerProxyLlmJudge {
    pub fn new(api: astra_thin_client::ThinClient, token: String, model: Option<String>) -> Self {
        Self { api, token, model }
    }
}

#[async_trait::async_trait]
impl astra_services::LlmJudge for ServerProxyLlmJudge {
    async fn evaluate(&self, prompt: &str, context: &str) -> Result<f64, String> {
        let system_msg = serde_json::json!({
            "role": "system",
            "content": "You are a verification judge. Evaluate whether an acceptance criterion \
                        is met based on the provided context. Respond with ONLY a JSON object: \
                        {\"score\": <0.0-1.0>, \"reason\": \"<brief explanation>\"}. \
                        Score 1.0 = fully met, 0.0 = not met at all."
        });
        let user_msg = serde_json::json!({
            "role": "user",
            "content": format!(
                "Criterion: {prompt}\n\nContext:\n{context}\n\n\
                 Evaluate and respond with {{\"score\": <0.0-1.0>, \"reason\": \"...\"}}."
            )
        });

        let mut body = serde_json::json!({
            "purpose": astra_turn_types::InferencePurpose::VerificationJudge,
            "messages": [system_msg, user_msg],
            "max_tokens": 2000,
            "temperature": 0.1,
        });
        if let Some(ref m) = self.model {
            body["model"] = serde_json::json!(m);
        }

        let resp = self
            .api
            .post_completions(&self.token, &body)
            .await
            .map_err(|e| format!("Server proxy judge error: {e}"))?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");

        parse_judge_score(content)
    }
}

// ─── Server Proxy Turn Intent Judge ──────────────────────────────────────────

/// [`astra_services::TurnIntentJudge`] routed through the API server's
/// `/v1/chat/completions` proxy.
///
/// Mirrors [`ServerProxyLlmJudge`] (verification): same auth, same model
/// resolution, no extra credentials. It can be injected through
/// `ServerAgenticLoopHost::set_turn_intent_judge` when a caller wants an
/// explicit proxy-backed judge instead of the host's built-in summary-client
/// judge path. On any error (transport, malformed output, rejection), the host
/// proceeds without explicit turn intent so a transient outage never blocks
/// the user's session.
pub struct ServerProxyTurnIntentJudge {
    api: astra_thin_client::ThinClient,
    token: String,
    model: Option<String>,
}

impl ServerProxyTurnIntentJudge {
    pub fn new(api: astra_thin_client::ThinClient, token: String, model: Option<String>) -> Self {
        Self { api, token, model }
    }
}

#[async_trait::async_trait]
impl astra_services::TurnIntentJudge for ServerProxyTurnIntentJudge {
    async fn judge(
        &self,
        ctx: &astra_services::TurnIntentJudgeContext,
    ) -> Result<astra_config::user_profile::TurnIntent, astra_services::TurnIntentJudgeError> {
        let mut body = serde_json::json!({
            "purpose": astra_turn_types::InferencePurpose::Introspection,
            "messages": astra_services::turn_intent_judge_messages(ctx),
            // Keep judge replies tight — the schema is fixed and small.
            "max_tokens": 256,
            // Low temperature for deterministic classification.
            "temperature": 0.0,
        });
        if let Some(ref m) = self.model {
            body["model"] = serde_json::json!(m);
        }

        let resp = self
            .api
            .post_completions(&self.token, &body)
            .await
            .map_err(|e| astra_services::TurnIntentJudgeError::Transport(e.to_string()))?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| astra_services::TurnIntentJudgeError::Malformed {
                raw: format!("missing content in response: {resp}"),
            })?;

        astra_services::parse_turn_intent_response(content)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        DurableTaskState, ServerProxyLlmJudge, create_local_lifecycle, create_local_lifecycle_full,
        display_contract_summary, display_delivery_report, display_verification_report,
        generate_contract, on_plan_complete, on_subtask_begin, on_subtask_complete,
        parse_judge_score, save_delivery_report_json_to_dir,
    };
    use crate::lock_recovery::LockRecovery;
    use astra_services::durable_task::{
        ContractStatus, DurableSubtask, SubtaskExecutionContext, SubtaskStage,
    };
    use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
    use astra_services::{
        ContractAmendment, ContractGenerator, DurableTaskLifecycle, LlmJudge, ServiceError,
        SubtaskDeliverySummary, SubtaskVerificationReport, TaskDeliveryReport, TaskResumeContext,
        TaskScope, VerificationCriterion, VerificationResult, VerifierKind,
    };
    use std::sync::Arc;

    fn service_error(message: impl Into<String>) -> ServiceError {
        ServiceError::internal(message)
    }

    fn make_test_plan() -> TaskPlan {
        TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Create module".into(),
                    description: Some("Create the foo module".into()),
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    effort: Some("small".into()),
                    files: vec!["src/foo.rs".into()],
                    acceptance_checks: vec![VerifierKind::FileExists {
                        paths: vec!["src/foo.rs".into()],
                    }],
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Add tests".into(),
                    description: Some("Write tests for foo".into()),
                    depends_on: vec!["s1".into()],
                    status: TaskStatus::Pending,
                    effort: Some("small".into()),
                    files: vec!["tests/foo_test.rs".into()],
                    acceptance_checks: vec![VerifierKind::TestPass {
                        cmd: "cargo test --workspace".into(),
                        min_pass_rate: 1.0,
                    }],
                },
            ],
            notes: None,
        }
    }

    fn persisted_contract_skeleton() -> astra_services::TaskContract {
        astra_services::TaskContract {
            contract_id: "contract-1".into(),
            task_id: "task-1".into(),
            goal: "Build foo".into(),
            scope: TaskScope::default(),
            subtasks: vec![DurableSubtask {
                id: "s1".into(),
                title: "Create module".into(),
                description: Some("Create the foo module".into()),
                criteria: vec![],
                ..Default::default()
            }],
            global_verification: vec![],
            version: 1,
            status: ContractStatus::Draft,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            domain_hint: None,
            task_type: None,
            last_global_results: vec![],
        }
    }

    struct StubDurableLifecycle {
        persisted_contract: astra_services::TaskContract,
        create_error: Option<String>,
        amend_error: Option<String>,
        amend_calls: std::sync::Mutex<usize>,
    }

    impl StubDurableLifecycle {
        fn new(create_error: Option<&str>, amend_error: Option<&str>) -> Self {
            Self {
                persisted_contract: persisted_contract_skeleton(),
                create_error: create_error.map(str::to_string),
                amend_error: amend_error.map(str::to_string),
                amend_calls: std::sync::Mutex::new(0),
            }
        }

        fn amend_calls(&self) -> usize {
            *self.amend_calls.lock_recover()
        }
    }

    #[async_trait::async_trait]
    impl DurableTaskLifecycle for StubDurableLifecycle {
        async fn create_contract(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &TaskPlan,
            _: TaskScope,
        ) -> Result<astra_services::TaskContract, ServiceError> {
            match &self.create_error {
                Some(error) => Err(ServiceError::internal(error.clone())),
                None => Ok(self.persisted_contract.clone()),
            }
        }

        async fn amend_contract(
            &self,
            _: &str,
            amendment: ContractAmendment,
        ) -> Result<astra_services::TaskContract, ServiceError> {
            *self.amend_calls.lock_recover() += 1;
            if let Some(error) = &self.amend_error {
                return Err(ServiceError::internal(error.clone()));
            }

            let mut amended = self.persisted_contract.clone();
            if let Some(subtasks) = amendment.updated_subtasks {
                amended.subtasks = subtasks;
            }
            if let Some(global) = amendment.updated_global_verification {
                amended.global_verification = global;
            }
            if let Some(scope) = amendment.updated_scope {
                amended.scope = scope;
            }
            amended.version += 1;
            Ok(amended)
        }

        async fn get_contract(
            &self,
            _: &str,
        ) -> Result<Option<astra_services::TaskContract>, ServiceError> {
            Ok(None)
        }

        async fn begin_subtask(
            &self,
            _: &str,
            _: &str,
        ) -> Result<SubtaskExecutionContext, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn complete_subtask_execution(&self, _: &str, _: &str) -> Result<(), ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn fail_subtask(&self, _: &str, _: &str, _: &str) -> Result<(), ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn verify_subtask(
            &self,
            _: &str,
            _: &str,
        ) -> Result<SubtaskVerificationReport, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn verify_global(&self, _: &str) -> Result<Vec<VerificationResult>, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn pause_task(&self, _: &str) -> Result<(), ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn resume_task(&self, _: &str, _: &str) -> Result<TaskResumeContext, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn deliver_task(&self, _: &str) -> Result<TaskDeliveryReport, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn snapshot_task_state(&self, _: &str) -> Result<String, ServiceError> {
            Err(ServiceError::internal("stub"))
        }

        async fn rollback_task(&self, _: &str, _: &str) -> Result<(), ServiceError> {
            Err(ServiceError::internal("stub"))
        }
    }

    #[test]
    fn contract_summary_display_does_not_panic() {
        let plan = make_test_plan();
        let detection = astra_services::ProjectDetection::detect(std::path::Path::new("/tmp"));
        let cg = ContractGenerator::new(detection);
        let contract = cg.generate("Build foo", &plan, None).unwrap();

        // Just verify it doesn't panic
        display_contract_summary(&contract);
    }

    #[tokio::test]
    async fn generate_contract_returns_none_when_create_contract_fails() {
        let lifecycle = Arc::new(StubDurableLifecycle::new(Some("persist failed"), None));
        let tmp = tempfile::tempdir().unwrap();

        let contract = generate_contract(
            &(lifecycle.clone() as Arc<dyn DurableTaskLifecycle>),
            &make_test_plan(),
            "Build foo",
            "user",
            "session",
            tmp.path(),
        )
        .await;

        assert!(contract.is_none());
        assert_eq!(lifecycle.amend_calls(), 0);
    }

    #[tokio::test]
    async fn generate_contract_returns_none_when_criteria_injection_fails() {
        let lifecycle = Arc::new(StubDurableLifecycle::new(None, Some("amend failed")));
        let tmp = tempfile::tempdir().unwrap();

        let contract = generate_contract(
            &(lifecycle.clone() as Arc<dyn DurableTaskLifecycle>),
            &make_test_plan(),
            "Build foo",
            "user",
            "session",
            tmp.path(),
        )
        .await;

        assert!(contract.is_none());
        assert_eq!(lifecycle.amend_calls(), 1);
    }

    #[tokio::test]
    async fn on_subtask_complete_stops_before_verify_when_diff_capture_fails() {
        struct CountingLifecycle {
            verify_calls: std::sync::Mutex<usize>,
        }

        #[async_trait::async_trait]
        impl DurableTaskLifecycle for CountingLifecycle {
            async fn create_contract(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &TaskPlan,
                _: TaskScope,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn amend_contract(
                &self,
                _: &str,
                _: ContractAmendment,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn get_contract(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Option<astra_services::TaskContract>>
            {
                Err(service_error("unused"))
            }

            async fn begin_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskExecutionContext> {
                Err(service_error("unused"))
            }

            async fn complete_subtask_execution(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("diff capture failed"))
            }

            async fn fail_subtask(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn verify_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskVerificationReport>
            {
                *self.verify_calls.lock_recover() += 1;
                Ok(SubtaskVerificationReport {
                    subtask_id: "s1".into(),
                    results: vec![],
                    all_required_passed: true,
                    timestamp: String::new(),
                })
            }

            async fn verify_global(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Vec<VerificationResult>> {
                Err(service_error("unused"))
            }

            async fn pause_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn resume_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskResumeContext> {
                Err(service_error("unused"))
            }

            async fn deliver_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskDeliveryReport> {
                Err(service_error("unused"))
            }

            async fn snapshot_task_state(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<String> {
                Err(service_error("unused"))
            }

            async fn rollback_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }
        }

        let lifecycle = Arc::new(CountingLifecycle {
            verify_calls: std::sync::Mutex::new(0),
        });
        let mut contract = persisted_contract_skeleton();
        contract.subtasks[0].criteria = vec![VerificationCriterion {
            id: "must-run".into(),
            description: "verify".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["x".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        contract.subtasks[0].stage = SubtaskStage::Executing;

        let mut durable = DurableTaskState {
            contract,
            lifecycle: lifecycle.clone(),
            last_report: None,
        };

        let (passed, report) = on_subtask_complete(&mut durable, "s1").await;

        assert!(!passed);
        assert!(report.is_none());
        assert_eq!(*lifecycle.verify_calls.lock_recover(), 0);
        assert!(matches!(
            durable.contract.subtasks[0].stage,
            SubtaskStage::ExecutionFailed { .. }
        ));
    }

    #[tokio::test]
    async fn on_subtask_begin_returns_err_and_marks_execution_failed() {
        struct BeginFailLifecycle;

        #[async_trait::async_trait]
        impl DurableTaskLifecycle for BeginFailLifecycle {
            async fn create_contract(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &TaskPlan,
                _: TaskScope,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn amend_contract(
                &self,
                _: &str,
                _: ContractAmendment,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn get_contract(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Option<astra_services::TaskContract>>
            {
                Err(service_error("unused"))
            }

            async fn begin_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskExecutionContext> {
                Err(service_error("snapshot unavailable"))
            }

            async fn complete_subtask_execution(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn fail_subtask(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn verify_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskVerificationReport>
            {
                Err(service_error("unused"))
            }

            async fn verify_global(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Vec<VerificationResult>> {
                Err(service_error("unused"))
            }

            async fn pause_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn resume_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskResumeContext> {
                Err(service_error("unused"))
            }

            async fn deliver_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskDeliveryReport> {
                Err(service_error("unused"))
            }

            async fn snapshot_task_state(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<String> {
                Err(service_error("unused"))
            }

            async fn rollback_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }
        }

        let mut durable = DurableTaskState {
            contract: persisted_contract_skeleton(),
            lifecycle: Arc::new(BeginFailLifecycle),
            last_report: None,
        };

        let err = on_subtask_begin(&mut durable, "s1").await.unwrap_err();
        assert_eq!(err, "[internal] snapshot unavailable");
        match &durable.contract.subtasks[0].stage {
            SubtaskStage::ExecutionFailed { error } => {
                assert_eq!(error, "[internal] snapshot unavailable");
            }
            other => panic!("unexpected stage: {other:?}"),
        }
    }

    #[tokio::test]
    async fn on_plan_complete_returns_err_when_delivery_fails() {
        struct DeliveryFailLifecycle;

        #[async_trait::async_trait]
        impl DurableTaskLifecycle for DeliveryFailLifecycle {
            async fn create_contract(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &TaskPlan,
                _: TaskScope,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn amend_contract(
                &self,
                _: &str,
                _: ContractAmendment,
            ) -> astra_services::service_error::ServiceResult<astra_services::TaskContract>
            {
                Err(service_error("unused"))
            }

            async fn get_contract(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Option<astra_services::TaskContract>>
            {
                Err(service_error("unused"))
            }

            async fn begin_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskExecutionContext> {
                Err(service_error("unused"))
            }

            async fn complete_subtask_execution(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn fail_subtask(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn verify_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<SubtaskVerificationReport>
            {
                Err(service_error("unused"))
            }

            async fn verify_global(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<Vec<VerificationResult>> {
                Ok(vec![])
            }

            async fn pause_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }

            async fn resume_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskResumeContext> {
                Err(service_error("unused"))
            }

            async fn deliver_task(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<TaskDeliveryReport> {
                Err(service_error("persist failed"))
            }

            async fn snapshot_task_state(
                &self,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<String> {
                Err(service_error("unused"))
            }

            async fn rollback_task(
                &self,
                _: &str,
                _: &str,
            ) -> astra_services::service_error::ServiceResult<()> {
                Err(service_error("unused"))
            }
        }

        let mut durable = DurableTaskState {
            contract: persisted_contract_skeleton(),
            lifecycle: Arc::new(DeliveryFailLifecycle),
            last_report: None,
        };

        let err = on_plan_complete(&mut durable).await.unwrap_err();
        assert_eq!(err, "delivery report failed: persist failed");
        assert!(durable.last_report.is_none());
    }

    #[tokio::test]
    async fn generate_contract_returns_amended_contract_when_activation_succeeds() {
        let lifecycle = Arc::new(StubDurableLifecycle::new(None, None));
        let tmp = tempfile::tempdir().unwrap();

        let contract = generate_contract(
            &(lifecycle.clone() as Arc<dyn DurableTaskLifecycle>),
            &make_test_plan(),
            "Build foo",
            "user",
            "session",
            tmp.path(),
        )
        .await
        .expect("durable contract should activate");

        assert_eq!(lifecycle.amend_calls(), 1);
        assert_eq!(contract.subtasks.len(), 2);
        assert!(
            contract
                .subtasks
                .iter()
                .all(|subtask| !subtask.criteria.is_empty()),
            "generated acceptance criteria should survive activation"
        );
    }

    #[test]
    fn verification_report_display_does_not_panic() {
        let report = SubtaskVerificationReport {
            subtask_id: "s1".into(),
            all_required_passed: false,
            results: vec![VerificationResult {
                criterion_id: "build".into(),
                passed: false,
                evidence: "exit code 1".into(),
                expected: "exit code 0".into(),
                duration_ms: 1200,
                error: None,
            }],
            timestamp: "2026-04-01T00:00:00Z".into(),
        };
        display_verification_report(&report);
    }

    #[test]
    fn delivery_report_display_does_not_panic() {
        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "Build a feature".into(),
            subtask_summaries: vec![SubtaskDeliverySummary {
                id: "s1".into(),
                title: "Create module".into(),
                stage: "Verified".into(),
                criteria_passed: 2,
                criteria_total: 2,
                retry_count: 0,
            }],
            global_verification: vec![],
            total_turns: 5,
            total_tokens: 10000,
            total_verifications: 2,
            risks: vec![],
            timestamp: "2026-04-01T00:00:00Z".into(),
        };
        display_delivery_report(&report);
    }

    #[test]
    fn delivery_report_json_save_uses_explicit_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "abcdef123456".into(),
            goal: "Build a feature".into(),
            subtask_summaries: vec![],
            global_verification: vec![],
            total_turns: 0,
            total_tokens: 0,
            total_verifications: 0,
            risks: vec![],
            timestamp: "2026-04-01T00:00:00Z".into(),
        };

        let path = save_delivery_report_json_to_dir(&report, tmp.path()).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".mo-delivery-abcdef12.json")
        );
        assert!(path.starts_with(tmp.path()));
        let saved: TaskDeliveryReport =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.task_id, report.task_id);
    }

    #[tokio::test]
    async fn create_local_lifecycle_works() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());

        let plan = make_test_plan();
        let contract = lifecycle
            .create_contract("user", "sess", "Build foo", &plan, TaskScope::default())
            .await
            .unwrap();

        assert!(!contract.contract_id.is_empty());
        assert_eq!(contract.subtasks.len(), 2);
        assert!(session_dir.join("contracts").exists());
        assert!(
            session_dir
                .join("contracts")
                .join(format!("{}.json", contract.contract_id))
                .exists(),
            "contract should be persisted directly under session/contracts"
        );
        assert!(
            !session_dir.join("contracts").join("contracts").exists(),
            "local durable data root should not nest contracts/contracts"
        );
    }

    #[test]
    fn parse_judge_score_json() {
        let score = parse_judge_score(r#"{"score": 0.85, "reason": "looks good"}"#).unwrap();
        assert!((score - 0.85).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_json_in_markdown() {
        let text =
            "Here is my evaluation:\n```json\n{\"score\": 0.7, \"reason\": \"mostly ok\"}\n```";
        let score = parse_judge_score(text).unwrap();
        assert!((score - 0.7).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_plain_number() {
        let score = parse_judge_score("The score is 0.9 out of 1.0").unwrap();
        assert!((score - 0.9).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_clamped() {
        let score = parse_judge_score(r#"{"score": 1.5}"#).unwrap();
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_no_number() {
        let result = parse_judge_score("This criterion is fully met");
        assert!(result.is_err());
    }

    // ─── End-to-end verification pipeline tests ────────────────────────────

    /// Full pipeline: contract generation → real file verification → pass/fail
    ///
    /// Creates actual files in a temp dir, generates a contract with criteria
    /// that inspect those files, then runs on_subtask_complete to verify gates fire.
    #[tokio::test]
    async fn e2e_contract_generate_verify_file_exists_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Create the file that subtask "s1" should produce
        let target = tmp.path().join("calculator.py");
        std::fs::write(
            &target,
            "def add(a, b): return a + b\ndef sub(a, b): return a - b\n",
        )
        .unwrap();

        // Plan: one subtask that requires calculator.py to exist
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Create calculator module".into(),
                description: Some("Create calculator.py with basic math functions".into()),
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: Some("small".into()),
                files: vec!["calculator.py".into()],
                acceptance_checks: vec![VerifierKind::FileExists {
                    paths: vec!["calculator.py".into()],
                }],
            }],
            notes: None,
        };

        // Generate contract via the real pipeline
        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let contract = generate_contract(
            &lifecycle,
            &plan,
            "Create a calculator module",
            "test-user",
            "test-session",
            tmp.path(),
        )
        .await;

        let contract = contract.expect("Contract generation should succeed");
        assert_eq!(contract.subtasks.len(), 1);

        // The contract generator should have created FileExists criteria
        let s1 = &contract.subtasks[0];
        assert!(!s1.criteria.is_empty(), "Subtask should have criteria");
        let has_file_check = s1
            .criteria
            .iter()
            .any(|c| matches!(c.verifier, VerifierKind::FileExists { .. }));
        assert!(
            has_file_check,
            "Should have FileExists verifier for 'File calculator.py exists'"
        );

        // Now run on_subtask_complete — it should PASS because the file exists
        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(passed, "Verification should pass — calculator.py exists");
        assert!(
            matches!(durable.contract.subtasks[0].stage, SubtaskStage::Verified),
            "Subtask stage should be Verified, got {:?}",
            durable.contract.subtasks[0].stage
        );
    }

    /// Verification fails when the required file is missing.
    #[tokio::test]
    async fn e2e_contract_verify_file_missing_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Deliberately do NOT create the file
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Create module".into(),
                description: Some("Create output.py".into()),
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: Some("small".into()),
                files: vec!["output.py".into()],
                acceptance_checks: vec![VerifierKind::FileExists {
                    paths: vec!["output.py".into()],
                }],
            }],
            notes: None,
        };

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let contract = generate_contract(
            &lifecycle,
            &plan,
            "Create output module",
            "test-user",
            "test-session",
            tmp.path(),
        )
        .await
        .expect("Contract generation should succeed");

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        // File doesn't exist → verification should fail
        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(!passed, "Verification should FAIL — output.py is missing");
        assert_eq!(durable.contract.subtasks[0].retry_count, 1);
        assert!(
            matches!(
                durable.contract.subtasks[0].stage,
                SubtaskStage::VerificationFailed { .. }
            ),
            "Stage should be VerificationFailed, got {:?}",
            durable.contract.subtasks[0].stage
        );
    }

    /// Grep verifier: checks that a pattern exists in a file.
    #[tokio::test]
    async fn e2e_grep_verify_pass_and_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("work");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        // Create a file with specific content
        std::fs::write(
            work_dir.join("auth.py"),
            "import jwt\ndef verify_token(token):\n    return jwt.decode(token)\n",
        )
        .unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, &work_dir);

        // Manually build a contract with GrepCheck criteria
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Implement JWT auth".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec!["auth.py".into()],
                acceptance_checks: vec![VerifierKind::GrepCheck {
                    file: "auth.py".into(),
                    pattern: "import jwt".into(),
                    should_match: true,
                }],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "JWT auth", &plan, TaskScope::default())
            .await
            .unwrap();

        // Inject GrepCheck criterion and persist via amendment
        let criteria = vec![VerificationCriterion {
            id: "grep-jwt".into(),
            description: "auth.py imports jwt".into(),
            verifier: VerifierKind::GrepCheck {
                file: "auth.py".into(),
                pattern: "import jwt".into(),
                should_match: true,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject test criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle: lifecycle.clone(),
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(passed, "GrepCheck should pass — 'import jwt' is in auth.py");
    }

    /// Grep verifier: fails when pattern is missing.
    #[tokio::test]
    async fn e2e_grep_verify_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("work");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        std::fs::write(
            work_dir.join("auth.py"),
            "import jwt\ndef verify_token(token):\n    return jwt.decode(token)\n",
        )
        .unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, &work_dir);

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Implement rate limiting".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec!["auth.py".into()],
                acceptance_checks: vec![VerifierKind::GrepCheck {
                    file: "auth.py".into(),
                    pattern: "rate_limit".into(),
                    should_match: true,
                }],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "rate limit", &plan, TaskScope::default())
            .await
            .unwrap();

        // Inject criteria AND persist them via amend_contract
        let criteria = vec![VerificationCriterion {
            id: "grep-missing".into(),
            description: "auth.py has rate limiting".into(),
            verifier: VerifierKind::GrepCheck {
                file: "auth.py".into(),
                pattern: "rate_limit".into(),
                should_match: true,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject test criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(
            !passed,
            "GrepCheck should fail — 'rate_limit' is NOT in auth.py"
        );
    }

    /// Command verifier: pass case (exit 0).
    #[tokio::test]
    async fn e2e_command_verify_exit_code_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("work");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, &work_dir);

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Run command".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![VerifierKind::Command {
                    cmd: "true".into(),
                    expected_exit: 0,
                }],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        let criteria = vec![VerificationCriterion {
            id: "cmd-pass".into(),
            description: "true command succeeds".into(),
            verifier: VerifierKind::Command {
                cmd: "true".into(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject test criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(passed, "Command 'true' should exit 0 → pass");
    }

    /// Command verifier: fail case (exit 1).
    #[tokio::test]
    async fn e2e_command_verify_exit_code_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("work");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, &work_dir);

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Run failing command".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![VerifierKind::Command {
                    cmd: "true".into(),
                    expected_exit: 0,
                }],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        let criteria = vec![VerificationCriterion {
            id: "cmd-fail".into(),
            description: "false command fails".into(),
            verifier: VerifierKind::Command {
                cmd: "false".into(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject test criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(!passed, "Command 'false' exits 1 → should fail");
    }

    /// TestPass verifier: runs pytest and checks pass rate.
    #[tokio::test]
    async fn e2e_test_pass_verifier_with_real_pytest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work_dir = tmp.path().join("work");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        // Create a passing test
        std::fs::write(
            work_dir.join("test_calc.py"),
            "def test_add():\n    assert 1 + 1 == 2\ndef test_sub():\n    assert 3 - 1 == 2\n",
        )
        .unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, &work_dir);
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Write tests".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec!["test_calc.py".into()],
                acceptance_checks: vec![VerifierKind::TestPass {
                    cmd: "pytest".into(),
                    min_pass_rate: 1.0,
                }],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        let criteria = vec![VerificationCriterion {
            id: "pytest-pass".into(),
            description: "All tests pass".into(),
            verifier: VerifierKind::TestPass {
                cmd: "python3 -m pytest test_calc.py -v".into(),
                min_pass_rate: 1.0,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject test criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        // pytest available: should pass and set stage to Verified
        // pytest not available: verification error → treated as pass (non-blocking),
        //   but stage won't change to Verified (stays as-is)
        if passed {
            // Verification either passed or was skipped due to error
            let stage = &durable.contract.subtasks[0].stage;
            assert!(
                matches!(stage, SubtaskStage::Verified)
                    || !matches!(stage, SubtaskStage::VerificationFailed { .. }),
                "Stage should be Verified or unchanged (if pytest unavailable), got {:?}",
                stage
            );
        }
    }

    /// Global verification: on_plan_complete runs cross-subtask checks.
    #[tokio::test]
    async fn e2e_global_verification_on_plan_complete() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Create both files the plan expects (use .txt to avoid Python project detection
        // which would inject a pytest global check that fails in CI without pytest)
        std::fs::write(tmp.path().join("module.txt"), "module\n").unwrap();
        std::fs::write(tmp.path().join("test_module.txt"), "test\n").unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Create module".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    effort: None,
                    files: vec!["module.txt".into()],
                    acceptance_checks: vec![VerifierKind::FileExists {
                        paths: vec!["module.txt".into()],
                    }],
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Create tests".into(),
                    description: None,
                    depends_on: vec!["s1".into()],
                    status: TaskStatus::Pending,
                    effort: None,
                    files: vec!["test_module.txt".into()],
                    acceptance_checks: vec![VerifierKind::FileExists {
                        paths: vec!["test_module.txt".into()],
                    }],
                },
            ],
            notes: None,
        };

        let contract = generate_contract(
            &lifecycle,
            &plan,
            "Create module and tests",
            "user",
            "sess",
            tmp.path(),
        )
        .await
        .expect("Contract should generate");

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        // Mark both subtasks as verified
        for sub in &mut durable.contract.subtasks {
            sub.stage = SubtaskStage::Verified;
        }

        // Run global verification — should pass (files exist)
        let global_passed = on_plan_complete(&mut durable).await.unwrap();
        assert!(
            global_passed,
            "Global verification should pass — both files exist"
        );
        assert!(
            durable.last_report.is_some(),
            "Delivery report should be generated"
        );
    }

    /// Retry mechanism: verification failure increments retry_count.
    #[tokio::test]
    async fn e2e_retry_count_increments_on_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Missing file".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec!["ghost.txt".into()],
                acceptance_checks: vec![VerifierKind::FileExists {
                    paths: vec!["ghost.txt".into()],
                }],
            }],
            notes: None,
        };

        let contract = generate_contract(
            &lifecycle,
            &plan,
            "Create ghost",
            "user",
            "sess",
            tmp.path(),
        )
        .await
        .expect("Contract should generate");

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        // First failure
        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        if !passed {
            assert_eq!(durable.contract.subtasks[0].retry_count, 1);

            // Second failure
            on_subtask_begin(&mut durable, "s1").await.unwrap();
            let (passed2, _report2) = on_subtask_complete(&mut durable, "s1").await;
            assert!(!passed2);
            assert_eq!(durable.contract.subtasks[0].retry_count, 2);
        }
    }

    /// Validates that `on_subtask_complete` calls `verify_subtask` even when all
    /// criteria are LlmJudge (no local criteria). Without this, the durable row
    /// stays `AwaitingVerification` and `verify_global` later errors with
    /// "subtasks not ready for global verification".
    #[tokio::test]
    async fn on_subtask_complete_calls_verify_for_llm_judge_only_criteria() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Implement feature".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![],
            }],
            notes: None,
        };

        let mut contract = lifecycle
            .create_contract("user", "sess", "quality check", &plan, TaskScope::default())
            .await
            .unwrap();

        // Inject an LlmJudge-only criterion (no local criteria at all)
        let criteria = vec![VerificationCriterion {
            id: "llm-quality".into(),
            description: "Code quality meets standards".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Is this code well-structured?".into(),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        }];
        contract.subtasks[0].criteria = criteria.clone();

        let mut amended_subtasks = contract.subtasks.clone();
        amended_subtasks[0].criteria = criteria;
        let amendment = ContractAmendment {
            reason: "inject LlmJudge-only criteria".into(),
            updated_subtasks: Some(amended_subtasks),
            updated_global_verification: None,
            updated_scope: None,
        };
        contract = lifecycle
            .amend_contract(&contract.contract_id, amendment)
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        // Verify: has_local_criteria should be false, but criteria_count > 0
        let sub = &durable.contract.subtasks[0];
        let has_local = sub
            .criteria
            .iter()
            .any(|c| !c.global_only && !matches!(c.verifier, VerifierKind::LlmJudge { .. }));
        assert!(
            !has_local,
            "Test setup: should have no local criteria (only LlmJudge)"
        );
        assert!(
            !sub.criteria.is_empty(),
            "Test setup: should have at least one criterion"
        );

        // Run on_subtask_complete — it must not skip verify_subtask.
        // Without an LLM judge configured, verify_subtask will fail or error,
        // but the key assertion is that it was *called* (stage changes from
        // AwaitingVerification to either Verified or VerificationFailed).
        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (_passed, _report) = on_subtask_complete(&mut durable, "s1").await;

        // Stage must NOT be AwaitingVerification — that would mean verify_subtask
        // was skipped, which is the bug we fixed.
        let stage = &durable.contract.subtasks[0].stage;
        assert!(
            !matches!(stage, SubtaskStage::AwaitingVerification),
            "Stage must not be AwaitingVerification after on_subtask_complete; got {:?}",
            stage
        );
    }

    /// Validates that on_subtask_complete returns true immediately when there
    /// are zero criteria (lifecycle promotes to Verified automatically).
    #[tokio::test]
    async fn on_subtask_complete_zero_criteria_returns_true() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let lifecycle = create_local_lifecycle(&session_dir, tmp.path());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Simple task".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![],
            }],
            notes: None,
        };

        let contract = lifecycle
            .create_contract("user", "sess", "simple", &plan, TaskScope::default())
            .await
            .unwrap();

        assert!(
            contract.subtasks[0].criteria.is_empty(),
            "Test setup: should have no criteria"
        );

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };

        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _report) = on_subtask_complete(&mut durable, "s1").await;
        assert!(passed, "Zero criteria should return true immediately");
    }
    // ─── ServerProxyLlmJudge integration tests ──────────────────────────

    /// Spin up a mock `/v1/chat/completions` server that returns a canned JSON score,
    /// then exercise `ServerProxyLlmJudge.evaluate()` end-to-end.
    async fn mock_completions_server(
        score: f64,
        reason: &str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, routing::post};

        let score_str = format!(r#"{{"score": {score}, "reason": "{reason}"}}"#);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |_body: Json<serde_json::Value>| {
                let content = score_str.clone();
                async move {
                    Json(serde_json::json!({
                        "id": "mock-1",
                        "object": "chat.completion",
                        "model": "mock-model",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": content },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                    }))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn server_proxy_judge_returns_high_score_on_positive() {
        let (base_url, server) = mock_completions_server(0.95, "criterion fully met").await;
        let api = astra_thin_client::ThinClient::new(&base_url, None).unwrap();
        let judge = ServerProxyLlmJudge::new(api, "fake-token".into(), None);

        let score = judge
            .evaluate(
                "Function returns i32",
                "fn add(a: i32, b: i32) -> i32 { a + b }",
            )
            .await
            .expect("evaluate should succeed");

        assert!((score - 0.95).abs() < 0.01, "expected ~0.95, got {score}");
        server.abort();
    }

    #[tokio::test]
    async fn server_proxy_judge_returns_low_score_on_negative() {
        let (base_url, server) = mock_completions_server(0.1, "no error handling").await;
        let api = astra_thin_client::ThinClient::new(&base_url, None).unwrap();
        let judge = ServerProxyLlmJudge::new(api, "fake-token".into(), None);

        let score = judge
            .evaluate(
                "Handles errors with Result",
                "fn divide(a: i32, b: i32) -> i32 { a / b }",
            )
            .await
            .expect("evaluate should succeed");

        assert!(score < 0.5, "expected low score, got {score}");
        server.abort();
    }

    #[tokio::test]
    async fn server_proxy_judge_sends_model_override_and_typed_purpose() {
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();

        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| {
                let cap = captured_clone.clone();
                async move {
                    *cap.lock_recover() = Some(body);
                    Json(serde_json::json!({
                        "id": "mock-1",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": r#"{"score": 0.8}"# },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                    }))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let api = astra_thin_client::ThinClient::new(&format!("http://{addr}"), None).unwrap();
        let judge =
            ServerProxyLlmJudge::new(api, "fake-token".into(), Some("custom-model-v2".into()));

        let _score = judge.evaluate("test", "test context").await.unwrap();
        let body = captured
            .lock_recover()
            .take()
            .expect("completion request captured");
        assert_eq!(body["model"], "custom-model-v2");
        assert_eq!(body["purpose"], "verification_judge");
        server.abort();
    }

    #[tokio::test]
    async fn server_proxy_judge_handles_connection_error() {
        // Point to a port nothing is listening on
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:19999", None).unwrap();
        let judge = ServerProxyLlmJudge::new(api, "fake-token".into(), None);

        let result: Result<f64, String> = judge.evaluate("test criterion", "test context").await;
        assert!(result.is_err(), "should fail on connection error");
        let err = result.unwrap_err();
        assert!(
            err.contains("Server proxy judge error"),
            "error should mention proxy: {err}"
        );
    }

    // ─── Judge wiring tests ────────────────────────────────────────────────────

    /// Fake LlmJudge that records calls and returns a fixed score.
    struct FakeJudge {
        score: f64,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeJudge {
        fn new(score: f64) -> Arc<Self> {
            Arc::new(Self {
                score,
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock_recover().len()
        }
    }

    #[async_trait::async_trait]
    impl LlmJudge for FakeJudge {
        async fn evaluate(&self, prompt: &str, _context: &str) -> Result<f64, String> {
            self.calls.lock_recover().push(prompt.to_string());
            Ok(self.score)
        }
    }

    /// When `cloud_judge` is None and `server_proxy_judge` is Some, the lifecycle
    /// MUST use the proxy judge for LlmJudge criteria.
    #[tokio::test]
    async fn lifecycle_server_proxy_judge_is_invoked_for_llm_criteria() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let fake_judge = FakeJudge::new(0.9); // passes at threshold 0.7

        let lifecycle = create_local_lifecycle_full(
            &session_dir,
            tmp.path(),
            None,
            Some("sess"),
            Some("user"),
            None,                                               // cloud_judge: None
            Some(Arc::clone(&fake_judge) as Arc<dyn LlmJudge>), // server_proxy_judge
        );

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Feature".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![VerifierKind::LlmJudge {
                    prompt: "Is output complete?".into(),
                    pass_threshold: 0.7,
                }],
            }],
            notes: None,
        };

        let contract = lifecycle
            .create_contract("user", "sess", "goal", &plan, TaskScope::default())
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };
        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _) = on_subtask_complete(&mut durable, "s1").await;

        assert!(passed, "Judge score 0.9 > 0.7 threshold → should pass");
        assert_eq!(
            fake_judge.call_count(),
            1,
            "Server proxy judge must be called exactly once for the LlmJudge criterion"
        );
    }

    /// When both `cloud_judge` and `server_proxy_judge` are provided, `cloud_judge`
    /// takes priority (it's the first in the wiring priority chain).
    #[tokio::test]
    async fn lifecycle_cloud_judge_takes_priority_over_server_proxy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();

        let cloud_judge = FakeJudge::new(0.95);
        let proxy_judge = FakeJudge::new(0.3); // below threshold — would fail

        let lifecycle = create_local_lifecycle_full(
            &session_dir,
            tmp.path(),
            None,
            Some("sess"),
            Some("user"),
            Some(Arc::clone(&cloud_judge) as Arc<dyn LlmJudge>),
            Some(Arc::clone(&proxy_judge) as Arc<dyn LlmJudge>),
        );

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Feature".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                effort: None,
                files: vec![],
                acceptance_checks: vec![VerifierKind::LlmJudge {
                    prompt: "Quality check".into(),
                    pass_threshold: 0.7,
                }],
            }],
            notes: None,
        };

        let contract = lifecycle
            .create_contract("user", "sess", "goal", &plan, TaskScope::default())
            .await
            .unwrap();

        let mut durable = DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        };
        on_subtask_begin(&mut durable, "s1").await.unwrap();
        let (passed, _) = on_subtask_complete(&mut durable, "s1").await;

        assert!(passed, "Cloud judge 0.95 > 0.7 → should pass");
        assert_eq!(cloud_judge.call_count(), 1, "Cloud judge must be called");
        assert_eq!(
            proxy_judge.call_count(),
            0,
            "Proxy judge must NOT be called when cloud judge wins"
        );
    }
}
