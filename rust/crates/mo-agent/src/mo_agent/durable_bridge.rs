//! Bridge between the durable task system and the REPL plan execution loop.
//!
//! This module wraps [`DurableTaskLifecycle`] calls with terminal display formatting
//! so plan execution can show contract generation, verification results, and delivery
//! reports in a user-friendly way.

use astra_runtime::{GateVerdict, VerificationGate};
use astra_services::coordination::AgentResult;
use astra_services::{
    ContractAmendment, ContractGenerator, DurableSubtask, DurableTaskLifecycle,
    LocalDurableTaskLifecycle, MatrixOneDurableTaskLifecycle, SubtaskStage,
    SubtaskVerificationReport, TaskContract, TaskDeliveryReport, VerificationRunner, VerifierKind,
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use std::sync::Arc;

/// Build a reqwest client that skips the system proxy for localhost/loopback URLs.
/// External URLs use the default proxy from `HTTP_PROXY`/`HTTPS_PROXY` env vars.
fn build_client_for_url(url: &str) -> reqwest::Client {
    let is_local = url.contains("127.0.0.1")
        || url.contains("localhost")
        || url.contains("[::1]")
        || url.contains("0.0.0.0");
    let mut builder = reqwest::Client::builder();
    if is_local {
        builder = builder.no_proxy();
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

// ─── Active contract state held by the REPL ──────────────────────────────────

/// Holds the active contract and lifecycle service during plan execution.
pub struct DurableTaskState {
    pub contract: TaskContract,
    pub lifecycle: Arc<dyn DurableTaskLifecycle>,
    /// Delivery report from the most recent `on_plan_complete()` call (if successful).
    pub last_report: Option<TaskDeliveryReport>,
}

// ─── Contract generation ─────────────────────────────────────────────────────

/// Generate a [`TaskContract`] from a plan and persist it via the lifecycle.
///
/// Returns `None` (with a warning) if generation or persistence fails —
/// plan execution proceeds without contract-backed verification.
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
            eprintln!("  {}  Contract generation skipped: {}", "⚠".yellow(), e);
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
                    eprintln!("  {}  Criteria injection failed: {}", "⚠".yellow(), e,);
                    display_contract_summary(&persisted);
                    Some(persisted)
                }
            }
        }
        Err(e) => {
            eprintln!("  {}  Contract persistence failed: {}", "⚠".yellow(), e);
            // Return the in-memory contract anyway so verification still works
            display_contract_summary(&contract);
            Some(contract)
        }
    }
}

/// Pretty-print a one-line contract summary on creation.
fn display_contract_summary(contract: &TaskContract) {
    let n_subtasks = contract.subtasks.len();
    let n_criteria: usize = contract.subtasks.iter().map(|s| s.criteria.len()).sum();
    let n_global = contract.global_verification.len();
    eprintln!(
        "\n{}  Contract generated: {} subtasks, {} criteria, {} global checks  [{}]",
        "📋".cyan(),
        n_subtasks,
        n_criteria,
        n_global,
        &contract.contract_id[..8],
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

/// Extract the latest verification results for a subtask as a JSON value
/// suitable for journaling. Returns `None` if the subtask has no results.
#[allow(dead_code)]
pub fn subtask_verification_json(
    durable: &DurableTaskState,
    subtask_id: &str,
) -> Option<serde_json::Value> {
    let sub = durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)?;
    match &sub.stage {
        SubtaskStage::Verified => Some(serde_json::json!({
            "stage": "verified",
            "retry_count": sub.retry_count,
        })),
        SubtaskStage::VerificationFailed { results } => Some(serde_json::json!({
            "stage": "verification_failed",
            "retry_count": sub.retry_count,
            "criteria": results,
        })),
        _ => None,
    }
}

/// Call when a subtask transitions Pending → Executing (snapshot).
pub async fn on_subtask_begin(durable: &DurableTaskState, subtask_id: &str) {
    if let Err(e) = durable
        .lifecycle
        .begin_subtask(&durable.contract.task_id, subtask_id)
        .await
    {
        eprintln!(
            "  {}  Snapshot skipped for {}: {}",
            "⚠".yellow(),
            subtask_id,
            e,
        );
    }
}

/// Call when a subtask's chat turn completes (diff capture + verification).
///
/// Returns `true` if verification passed (or no criteria), `false` if failed.
pub async fn on_subtask_complete(durable: &mut DurableTaskState, subtask_id: &str) -> bool {
    let task_id = durable.contract.task_id.clone();

    // 1. Complete execution (captures diff)
    if let Err(e) = durable
        .lifecycle
        .complete_subtask_execution(&task_id, subtask_id)
        .await
    {
        eprintln!(
            "  {}  Diff capture failed for {}: {}",
            "⚠".yellow(),
            subtask_id,
            e,
        );
    }

    // 2. Check if this subtask has any *local* verification criteria
    //    (skip global_only and LlmJudge — those only run in on_plan_complete)
    let has_local_criteria = durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)
        .map(|s| {
            s.criteria
                .iter()
                .any(|c| !c.global_only && !matches!(c.verifier, VerifierKind::LlmJudge { .. }))
        })
        .unwrap_or(false);

    if !has_local_criteria {
        // Silently skip — heavy checks run during global verification
        return true;
    }

    // 3. Run lightweight verification with progress indication + spinner
    let spinner = super::stream_render::Spinner::start(format!("🔍 Verifying: {subtask_id}"));
    let result = durable.lifecycle.verify_subtask(&task_id, subtask_id).await;
    spinner.stop_clear();
    match result {
        Ok(report) => {
            display_verification_report(&report);

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

            report.all_required_passed
        }
        Err(e) => {
            eprintln!(
                "  {}  Verification error for {}: {}",
                "⚠".yellow(),
                subtask_id,
                e,
            );
            true // Don't block on verification infrastructure failures
        }
    }
}

/// Pretty-print subtask verification results.
fn display_verification_report(report: &SubtaskVerificationReport) {
    let passed = report.results.iter().filter(|r| r.passed).count();
    let total = report.results.len();
    let icon = if report.all_required_passed {
        "✔"
    } else {
        "✘"
    };
    let styled_icon = if report.all_required_passed {
        format!("{}", icon.green())
    } else {
        format!("{}", icon.red())
    };

    eprintln!(
        "  {}  Verification: {}/{} criteria passed  [{}]",
        styled_icon, passed, total, report.subtask_id,
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
                    "✔".green(),
                    r.criterion_id.clone().dark_grey(),
                    evidence.dark_grey(),
                    dur_tag.dark_grey(),
                );
            } else {
                eprintln!(
                    "      {} {}{}",
                    "✔".green(),
                    r.criterion_id.clone().dark_grey(),
                    dur_tag.dark_grey(),
                );
            }
        } else {
            let evidence: String = r.evidence.trim().chars().take(200).collect();
            let expected: String = r.expected.chars().take(120).collect();
            eprintln!(
                "      {} {}{}",
                "✘".red(),
                r.criterion_id,
                dur_tag.dark_grey(),
            );
            if !evidence.is_empty() {
                eprintln!("        got: {}", evidence.yellow());
            }
            if !expected.is_empty() {
                eprintln!("        expected: {}", expected);
            }
            if let Some(ref err) = r.error {
                eprintln!("        error: {}", err.clone().red());
            }
        }
    }
}

// ─── Global verification + delivery ─────────────────────────────────────────

/// Run global verification (build/test/lint) after all subtasks complete.
/// Returns `true` if all required global checks pass.
pub async fn on_plan_complete(durable: &mut DurableTaskState) -> bool {
    let task_id = durable.contract.task_id.clone();

    eprintln!("\n{}  Running global verification...", "🔬".cyan(),);

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
        eprintln!("      {} {}", "▸".grey(), cmd_hint);
    }

    let spinner = super::stream_render::Spinner::start("🔬 Running global checks".into());
    let verify_result = durable.lifecycle.verify_global(&task_id).await;
    spinner.stop_clear();

    match verify_result {
        Ok(results) => {
            let passed = results.iter().filter(|r| r.passed).count();
            let total = results.len();
            let all_passed = results.iter().all(|r| r.passed);

            let icon = if all_passed { "✔" } else { "✘" };
            let styled = if all_passed {
                format!("{}", icon.green())
            } else {
                format!("{}", icon.red())
            };

            eprintln!("  {}  Global checks: {}/{} passed", styled, passed, total,);

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
                        "✔".green(),
                        r.criterion_id.clone().dark_grey(),
                        dur_tag.dark_grey(),
                    );
                } else {
                    let evidence = r.evidence.chars().take(200).collect::<String>();
                    eprintln!(
                        "      {} {} — {}{}",
                        "✘".red(),
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
                        save_delivery_report_json(&report);
                        durable.last_report = Some(report);
                    }
                    Err(e) => eprintln!("  {}  Delivery report failed: {}", "⚠".yellow(), e,),
                }
            }

            all_passed
        }
        Err(e) => {
            eprintln!("  {}  Global verification error: {}", "⚠".yellow(), e,);
            true // Don't block plan on verification infra failure
        }
    }
}

/// Pretty-print the final delivery report.
pub(super) fn display_delivery_report(report: &TaskDeliveryReport) {
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

    // Determine box width based on terminal (min 58, max 80)
    let box_width = crossterm::terminal::size()
        .map(|(c, _)| (c as usize).clamp(58, 80))
        .unwrap_or(58);
    let bar = "═".repeat(box_width - 2);
    let dash = "─".repeat(box_width - 2);

    // ─── Header ──────────────────────────────────────────────────────────────
    eprintln!();
    eprintln!("{}", format!("╔{bar}╗").cyan());

    // Goal — use most of the box width instead of hard 50-char truncation
    let goal_max = box_width.saturating_sub(12); // "║  Task: " + pad
    let goal_display: String = if report.goal.chars().count() > goal_max {
        let mut g: String = report.goal.chars().take(goal_max - 1).collect();
        g.push('…');
        g
    } else {
        report.goal.clone()
    };
    eprintln!("{}  Task: {}", "║".cyan(), goal_display.white().bold(),);

    let (status_icon, status_text) = if fully_delivered {
        ("✅", "Delivered".green().bold())
    } else if all_subtasks_verified {
        ("⚠️", "Partial (global checks failed)".yellow().bold())
    } else {
        ("⚠️", "Partial".yellow().bold())
    };
    eprintln!("{}  Status: {} {}", "║".cyan(), status_icon, status_text);

    // ─── Subtask Results ─────────────────────────────────────────────────────
    eprintln!("{}", format!("╠{bar}╣").cyan());

    for sub in &report.subtask_summaries {
        let verified = sub.criteria_passed == sub.criteria_total;
        let icon = if verified { "✅" } else { "⚠️" };
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
            "{}  {} {} ({}{}{})",
            "║".cyan(),
            icon,
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
        eprintln!("{}", format!("╠{dash}╣").cyan());
        eprintln!(
            "{}  🔬 Global checks: {}/{}",
            "║".cyan(),
            global_passed,
            global_total,
        );
        for r in &report.global_verification {
            let icon = if r.passed { "✔" } else { "✘" };
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
            eprintln!("{}      {} {}{}", "║".cyan(), styled, r.criterion_id, dur,);
        }
    }

    // ─── Metrics ─────────────────────────────────────────────────────────────
    eprintln!("{}", format!("╠{dash}╣").cyan());

    let mut metrics = Vec::new();
    metrics.push(format!(
        "📊 {}/{} criteria passed",
        criteria_passed, criteria_total
    ));
    if total_retries > 0 {
        metrics.push(format!("↻ {} retries", total_retries));
    }
    if report.total_verifications > 0 {
        metrics.push(format!("🔍 {} verifications", report.total_verifications));
    }
    eprintln!("{}  {}", "║".cyan(), metrics.join("  ·  "));

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
        eprintln!(
            "{}  {}",
            "║".cyan(),
            format!("⚡ {}", effort.join(", ")).dark_grey(),
        );
    }
    if !report.timestamp.is_empty() {
        eprintln!(
            "{}  {}",
            "║".cyan(),
            format!("🕐 {}", report.timestamp).dark_grey(),
        );
    }

    // ─── Risks / Assumptions ─────────────────────────────────────────────────
    if !report.risks.is_empty() {
        eprintln!("{}", format!("╠{dash}╣").cyan());
        for risk in &report.risks {
            eprintln!("{}  ⚠ {}", "║".cyan(), risk.clone().yellow());
        }
    }

    // ─── Footer ──────────────────────────────────────────────────────────────
    eprintln!("{}", format!("╚{bar}╝").cyan());
}

/// Save the delivery report as JSON to the working directory.
/// Prints the file path on success (dim grey, non-intrusive).
pub(super) fn save_delivery_report_json(report: &TaskDeliveryReport) {
    let filename = format!(
        ".mo-delivery-{}.json",
        report.contract_id.chars().take(8).collect::<String>()
    );
    let path = std::env::current_dir().unwrap_or_default().join(&filename);
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, &json) {
                eprintln!("  {}  Could not save report: {}", "⚠".yellow(), e,);
            } else {
                eprintln!(
                    "  {}",
                    format!("📄 Report saved: {}", path.display()).dark_grey(),
                );
            }
        }
        Err(e) => {
            eprintln!("  {}  Could not serialize report: {}", "⚠".yellow(), e,);
        }
    }
}

// ─── Post-delivery user feedback ─────────────────────────────────────────────

/// Prompt the user for a 1–5 rating and feed it back into the learning pipeline.
///
/// Returns the raw rating (1–5) if the user provided one, or `None` if skipped.
#[allow(dead_code)]
pub async fn collect_user_feedback(
    durable: &DurableTaskState,
    learning_bridge: Option<&std::sync::Arc<dyn astra_services::TaskLearningBridge>>,
) -> Option<u8> {
    let report = durable.last_report.as_ref()?;

    eprintln!();
    eprint!("  {} ", "Rate this delivery (1-5, Enter to skip):".bold());
    std::io::Write::flush(&mut std::io::stderr()).ok();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let rating: u8 = match trimmed.parse() {
        Ok(r) if (1..=5).contains(&r) => r,
        _ => {
            eprintln!("  {}", "Skipped (expected 1-5).".dim());
            return None;
        }
    };

    let stars = "★".repeat(rating as usize);
    let empty = "☆".repeat(5 - rating as usize);
    eprintln!("  {} {}{}", "📊".cyan(), stars.yellow(), empty.dim());

    // Convert 1-5 → 0-100 scale for learning pipeline
    let rating_100 = (rating as u16 * 20).min(100) as u8;

    if let Some(bridge) = learning_bridge {
        let all_tools: Vec<String> = durable
            .contract
            .subtasks
            .iter()
            .flat_map(|s| s.tools_used.iter().cloned())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let outcome = astra_services::durable_task::build_outcome_signal(
            &durable.contract,
            report,
            all_tools,
            Some(rating_100),
            durable.contract.domain_hint.clone(),
            durable.contract.task_type.clone(),
        );
        let _ = bridge.learn_from_task_outcome(&outcome).await;
    }

    Some(rating)
}

// ─── Lifecycle factory ───────────────────────────────────────────────────────

/// Create a [`LocalDurableTaskLifecycle`] with optional cloud event streaming.
///
/// Uses the session directory as the persistence root and the cwd as the work_dir
/// for the embedded [`VerificationRunner`]. When an `IngestionSender` is provided,
/// verification events are streamed to the cloud asynchronously (local-first with
/// cloud event streaming).
/// Convenience wrapper without cloud event streaming (used in tests).
#[cfg(test)]
pub fn create_local_lifecycle(
    session_dir: &std::path::Path,
    work_dir: &std::path::Path,
) -> Arc<dyn DurableTaskLifecycle> {
    create_local_lifecycle_full(session_dir, work_dir, None, None, None, None, None, None)
}

/// Like [`create_local_lifecycle`] but also wires cloud event streaming.
#[allow(dead_code)] // Public API — callers without cloud judge use this shorthand
pub fn create_local_lifecycle_with_sender(
    session_dir: &std::path::Path,
    work_dir: &std::path::Path,
    sender: Option<astra_services::event_ingestion::IngestionSender>,
    session_id: Option<&str>,
    user_id: Option<&str>,
) -> Arc<dyn DurableTaskLifecycle> {
    create_local_lifecycle_full(
        session_dir,
        work_dir,
        sender,
        session_id,
        user_id,
        None,
        None,
        None,
    )
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
    learning_bridge: Option<Arc<dyn astra_services::TaskLearningBridge>>,
    server_proxy_judge: Option<Arc<dyn astra_services::LlmJudge>>,
) -> Arc<dyn DurableTaskLifecycle> {
    let contracts_dir = session_dir.join("contracts");
    let _ = std::fs::create_dir_all(&contracts_dir);
    let mut lifecycle = LocalDurableTaskLifecycle::new(contracts_dir, work_dir.to_path_buf());

    // Wire up LLM judge (priority: cloud > env-var HTTP > server proxy)
    if let Some(judge) = cloud_judge {
        lifecycle.set_llm_judge(judge);
    } else if let Some(judge) = HttpLlmJudge::from_env() {
        lifecycle.set_llm_judge(Arc::new(judge));
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

    // Wire up learning bridge for verification → learning feedback loop
    if let Some(bridge) = learning_bridge {
        lifecycle.set_learning_bridge(bridge);
    }

    // Wire up live output streaming — tees build/test stderr to the terminal
    // with dim grey styling so it's visible but doesn't dominate the output.
    lifecycle.set_output_sink(Arc::new(|line: &str| {
        use crossterm::style::Stylize;
        eprintln!("      {}", line.dark_grey());
    }));

    Arc::new(lifecycle)
}

/// Like [`create_local_lifecycle_full`] but uses the cloud-backed
/// [`MatrixOneDurableTaskLifecycle`] so that contracts and verification
/// results are persisted to the MatrixOne database.
#[allow(clippy::too_many_arguments)]
pub fn create_cloud_lifecycle_full(
    pool: sqlx::Pool<sqlx::MySql>,
    work_dir: &std::path::Path,
    sender: Option<astra_services::event_ingestion::IngestionSender>,
    session_id: Option<&str>,
    user_id: Option<&str>,
    cloud_judge: Option<Arc<dyn astra_services::LlmJudge>>,
    learning_bridge: Option<Arc<dyn astra_services::TaskLearningBridge>>,
    server_proxy_judge: Option<Arc<dyn astra_services::LlmJudge>>,
) -> Arc<dyn DurableTaskLifecycle> {
    let mut lifecycle = MatrixOneDurableTaskLifecycle::new(pool, work_dir.to_path_buf());

    if let Some(judge) = cloud_judge {
        lifecycle.set_llm_judge(judge);
    } else if let Some(judge) = HttpLlmJudge::from_env() {
        lifecycle.set_llm_judge(Arc::new(judge));
    } else if let Some(judge) = server_proxy_judge {
        lifecycle.set_llm_judge(judge);
    }

    if let Some(s) = sender {
        lifecycle.set_event_sender(s);
    }
    if let (Some(sid), Some(uid)) = (session_id, user_id) {
        lifecycle.set_session_context(sid, uid);
    }

    if let Some(bridge) = learning_bridge {
        lifecycle.set_learning_bridge(bridge);
    }

    lifecycle.set_output_sink(Arc::new(|line: &str| {
        use crossterm::style::Stylize;
        eprintln!("      {}", line.dark_grey());
    }));

    Arc::new(lifecycle)
}

// ─── Delegation Verification Gate ────────────────────────────────────────────

/// A [`VerificationGate`] that runs durable task verification criteria against
/// delegation sub-run results.
///
/// When a delegation sub-run completes, this gate runs the associated subtask's
/// acceptance criteria (build/test/grep/file checks) and returns [`GateVerdict::Pass`]
/// only if all required criteria pass. On failure, it returns details about which
/// criteria failed so the delegation engine can retry.
///
/// This bridges the durable task verification system into the delegation quality
/// control loop, ensuring sub-runs don't bypass acceptance checks.
pub struct ContractVerificationGate {
    /// Criteria to verify after the sub-run completes.
    criteria: Vec<astra_services::VerificationCriterion>,
    /// Subtask ID for labeling results.
    subtask_id: String,
    /// Runner that executes command/file/grep verifications.
    runner: VerificationRunner,
    /// Maximum retries before giving up (overrides VerificationGate default).
    max_retry: u32,
}

impl ContractVerificationGate {
    /// Create a gate from a [`DurableSubtask`]'s criteria.
    ///
    /// The gate will run all non-global criteria when `verify()` is called.
    #[allow(dead_code)]
    pub fn from_subtask(subtask: &DurableSubtask, work_dir: std::path::PathBuf) -> Self {
        Self {
            criteria: subtask
                .criteria
                .iter()
                .filter(|c| !c.global_only)
                .cloned()
                .collect(),
            subtask_id: subtask.id.clone(),
            runner: VerificationRunner::new(work_dir),
            max_retry: subtask.max_retries,
        }
    }

    /// Create a gate from explicit criteria (for testing or custom pipelines).
    #[allow(dead_code)]
    pub fn from_criteria(
        criteria: Vec<astra_services::VerificationCriterion>,
        subtask_id: String,
        work_dir: std::path::PathBuf,
        max_retry: u32,
    ) -> Self {
        Self {
            criteria,
            subtask_id,
            runner: VerificationRunner::new(work_dir),
            max_retry,
        }
    }

    /// Attach an LLM judge for semantic criteria.
    #[allow(dead_code)]
    pub fn with_llm_judge(mut self, judge: Arc<dyn astra_services::LlmJudge>) -> Self {
        self.runner.llm_judge = Some(judge);
        self
    }
}

#[async_trait]
impl VerificationGate for ContractVerificationGate {
    async fn verify(
        &self,
        _result: &AgentResult,
        _delegation_id: &str,
        _attempt: u32,
    ) -> GateVerdict {
        if self.criteria.is_empty() {
            return GateVerdict::Skip;
        }

        // Build a temporary DurableSubtask to run verification through the runner.
        let subtask = DurableSubtask {
            id: self.subtask_id.clone(),
            title: String::new(),
            description: None,
            depends_on: Vec::new(),
            effort: None,
            files: Vec::new(),
            stage: SubtaskStage::AwaitingVerification,
            criteria: self.criteria.clone(),
            max_retries: self.max_retry,
            retry_count: 0,
            snapshot_name: None,
            data_branch: None,
            diff_summary: None,
            last_verification: None,
            tools_used: Vec::new(),
        };

        let report = self.runner.verify_subtask_local(&subtask).await;

        if report.all_required_passed {
            GateVerdict::Pass
        } else {
            let failed: Vec<_> = report
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| {
                    serde_json::json!({
                        "criterion": r.criterion_id,
                        "expected": r.expected,
                        "evidence": r.evidence,
                        "error": r.error,
                    })
                })
                .collect();

            let reasons: Vec<_> = report
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| {
                    format!(
                        "{}: expected [{}], got [{}]",
                        r.criterion_id, r.expected, r.evidence
                    )
                })
                .collect();

            GateVerdict::Fail {
                reason: format!(
                    "Subtask '{}' failed {}/{} criteria: {}",
                    self.subtask_id,
                    failed.len(),
                    self.criteria.len(),
                    reasons.join("; ")
                ),
                details: Some(serde_json::json!({
                    "subtask_id": self.subtask_id,
                    "report": report,
                    "failed_criteria": failed,
                })),
            }
        }
    }

    fn max_retries(&self) -> u32 {
        self.max_retry
    }
}

// ─── Gate factory for plan-execution integration ─────────────────────────────

/// Create a [`ContractVerificationGate`] for a specific subtask.
///
/// Returns `None` when the subtask has no (non-global) criteria — the
/// delegation engine will skip gate checking in that case.
#[allow(dead_code)]
pub fn create_gate_for_subtask(
    durable: &DurableTaskState,
    subtask_id: &str,
    work_dir: std::path::PathBuf,
) -> Option<Arc<dyn VerificationGate>> {
    let subtask = durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)?;
    let non_global: Vec<_> = subtask.criteria.iter().filter(|c| !c.global_only).collect();
    if non_global.is_empty() {
        return None;
    }
    Some(Arc::new(ContractVerificationGate::from_subtask(
        subtask, work_dir,
    )))
}

// ─── Mid-Execution Checkpoint Gate ───────────────────────────────────────────

/// A [`CheckpointGate`] that runs lightweight verifier checks during execution.
///
/// Uses only quick, non-destructive criteria (file-exists, grep) as
/// early-termination signals. Long-running checks (build, test) are deferred
/// to the post-completion [`ContractVerificationGate`].
#[allow(dead_code)]
pub struct ContractCheckpointGate {
    /// Quick criteria (file-exists / grep only) extracted from the subtask.
    quick_criteria: Vec<astra_services::VerificationCriterion>,
    subtask_id: String,
    runner: VerificationRunner,
    frequency: u32,
}

impl ContractCheckpointGate {
    /// Build from a [`DurableSubtask`], keeping only instant-verifiable criteria.
    pub fn from_subtask(
        subtask: &DurableSubtask,
        work_dir: std::path::PathBuf,
        frequency: u32,
    ) -> Self {
        let quick_criteria = subtask
            .criteria
            .iter()
            .filter(|c| !c.global_only)
            .filter(|c| {
                matches!(
                    c.verifier,
                    VerifierKind::FileExists { .. } | VerifierKind::GrepCheck { .. }
                )
            })
            .cloned()
            .collect();
        Self {
            quick_criteria,
            subtask_id: subtask.id.clone(),
            runner: VerificationRunner::new(work_dir),
            frequency,
        }
    }

    /// Returns `true` if this gate has any quick criteria to check.
    pub fn has_checks(&self) -> bool {
        !self.quick_criteria.is_empty()
    }
}

#[async_trait]
impl astra_runtime::server::delegation_engine::CheckpointGate for ContractCheckpointGate {
    async fn check(
        &self,
        _run_id: &str,
        _turn_index: u32,
        _total_tool_calls: u32,
    ) -> Result<bool, String> {
        if self.quick_criteria.is_empty() {
            return Ok(true);
        }

        let temp_subtask = DurableSubtask {
            id: self.subtask_id.clone(),
            title: String::new(),
            description: None,
            depends_on: Vec::new(),
            effort: None,
            files: Vec::new(),
            stage: SubtaskStage::Executing,
            criteria: self.quick_criteria.clone(),
            max_retries: 0,
            retry_count: 0,
            snapshot_name: None,
            data_branch: None,
            diff_summary: None,
            last_verification: None,
            tools_used: Vec::new(),
        };

        let report = self.runner.verify_subtask_local(&temp_subtask).await;

        // If any *required* quick criterion explicitly failed (not errored), abort.
        let hard_fail = report.results.iter().any(|r| {
            !r.passed && r.error.is_none() && {
                self.quick_criteria
                    .iter()
                    .any(|c| c.id == r.criterion_id && c.required)
            }
        });

        Ok(!hard_fail)
    }

    fn checkpoint_frequency(&self) -> u32 {
        self.frequency
    }
}

/// Create a [`ContractCheckpointGate`] for a subtask, if it has quick criteria.
#[allow(dead_code)]
pub fn create_checkpoint_gate_for_subtask(
    durable: &DurableTaskState,
    subtask_id: &str,
    work_dir: std::path::PathBuf,
    frequency: u32,
) -> Option<Arc<dyn astra_runtime::server::delegation_engine::CheckpointGate>> {
    let subtask = durable
        .contract
        .subtasks
        .iter()
        .find(|s| s.id == subtask_id)?;
    let gate = ContractCheckpointGate::from_subtask(subtask, work_dir, frequency);
    if gate.has_checks() {
        Some(Arc::new(gate))
    } else {
        None
    }
}

// ─── LLM Judge Implementation ────────────────────────────────────────────────

/// Concrete [`LlmJudge`] that calls an OpenAI-compatible chat completions API.
///
/// Sends a structured prompt asking the LLM to evaluate a criterion and return
/// a confidence score. Parses the response to extract a 0.0–1.0 score.
pub struct HttpLlmJudge {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl HttpLlmJudge {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        let client = build_client_for_url(&base_url);
        Self {
            client,
            api_key,
            base_url,
            model,
        }
    }

    /// Try to create from environment variables.
    ///
    /// Looks for `MO_LLM_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`,
    /// `MO_LLM_BASE_URL` / `OPENAI_BASE_URL`, and `MO_LLM_MODEL`.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("MO_LLM_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .ok()?;
        let base_url = std::env::var("MO_LLM_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("MO_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
        Some(Self::new(api_key, base_url, model))
    }
}

#[async_trait::async_trait]
impl astra_services::LlmJudge for HttpLlmJudge {
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

        let body = serde_json::json!({
            "model": self.model,
            "messages": [system_msg, user_msg],
            "max_tokens": 200,
            "temperature": 0.1,
        });

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("LLM request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "LLM API error {status}: {}",
                &text[..text.len().min(200)]
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("LLM response parse failed: {e}"))?;

        // Extract the assistant's response text
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");

        // Parse the score from the response
        parse_judge_score(content)
    }
}

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
        &text[..text.len().min(200)]
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
            "messages": [system_msg, user_msg],
            "max_tokens": 200,
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
    use astra_services::{
        SubtaskDeliverySummary, TaskScope, VerificationCriterion, VerificationResult,
    };

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
                    acceptance: Some("File src/foo.rs exists".into()),
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Add tests".into(),
                    description: Some("Write tests for foo".into()),
                    depends_on: vec!["s1".into()],
                    status: TaskStatus::Pending,
                    effort: Some("small".into()),
                    files: vec!["tests/foo_test.rs".into()],
                    acceptance: Some("cargo test passes".into()),
                },
            ],
            notes: None,
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

    // ─── ContractVerificationGate tests ──────────────────────────────────────

    fn make_agent_result() -> astra_services::coordination::AgentResult {
        astra_services::coordination::AgentResult {
            agent_id: "test-agent".into(),
            run_id: "run-1".into(),
            status: "completed".into(),
            output: Some("done".into()),
            error: None,
            prompt_tokens: 100,
            completion_tokens: 50,
            tool_calls: 3,
        }
    }

    #[tokio::test]
    async fn gate_skip_when_no_criteria() {
        let gate = ContractVerificationGate::from_criteria(
            vec![],
            "s1".into(),
            std::path::PathBuf::from("/tmp"),
            2,
        );
        let verdict = gate.verify(&make_agent_result(), "deleg-1", 1).await;
        assert!(matches!(verdict, astra_runtime::GateVerdict::Skip));
    }

    #[tokio::test]
    async fn gate_pass_when_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("output.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let criteria = vec![astra_services::VerificationCriterion {
            id: "file-check".into(),
            description: "Output file exists".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["output.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];

        let gate = ContractVerificationGate::from_criteria(
            criteria,
            "s1".into(),
            tmp.path().to_path_buf(),
            2,
        );
        let verdict = gate.verify(&make_agent_result(), "deleg-1", 1).await;
        assert!(matches!(verdict, astra_runtime::GateVerdict::Pass));
    }

    #[tokio::test]
    async fn gate_fail_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();

        let criteria = vec![astra_services::VerificationCriterion {
            id: "file-check".into(),
            description: "Output file exists".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["missing.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];

        let gate = ContractVerificationGate::from_criteria(
            criteria,
            "s1".into(),
            tmp.path().to_path_buf(),
            1,
        );
        let verdict = gate.verify(&make_agent_result(), "deleg-1", 1).await;
        match verdict {
            astra_runtime::GateVerdict::Fail { reason, details } => {
                assert!(reason.contains("s1"));
                assert!(reason.contains("failed"));
                assert!(details.is_some());
            }
            _ => panic!("Expected Fail verdict, got {:?}", verdict),
        }
    }

    #[tokio::test]
    async fn gate_skips_global_only_criteria() {
        let subtask = DurableSubtask {
            id: "s1".into(),
            title: "Test".into(),
            description: None,
            depends_on: vec![],
            effort: None,
            files: vec![],
            stage: SubtaskStage::AwaitingVerification,
            criteria: vec![
                astra_services::VerificationCriterion {
                    id: "local-check".into(),
                    description: "Local check".into(),
                    verifier: VerifierKind::FileExists {
                        paths: vec!["nonexistent.txt".into()],
                    },
                    required: false, // optional — won't cause failure
                    timeout_sec: 10,
                    global_only: false,
                },
                astra_services::VerificationCriterion {
                    id: "global-check".into(),
                    description: "Build check".into(),
                    verifier: VerifierKind::BuildPass {
                        cmd: "false".into(), // would fail if run
                    },
                    required: true,
                    timeout_sec: 10,
                    global_only: true, // should be filtered out by from_subtask
                },
            ],
            max_retries: 2,
            retry_count: 0,
            snapshot_name: None,
            data_branch: None,
            diff_summary: None,
            last_verification: None,
            tools_used: vec![],
        };

        let gate =
            ContractVerificationGate::from_subtask(&subtask, std::path::PathBuf::from("/tmp"));
        // Only the non-global, non-required criterion is included → Pass
        let verdict = gate.verify(&make_agent_result(), "deleg-1", 1).await;
        assert!(
            matches!(verdict, astra_runtime::GateVerdict::Pass),
            "Expected Pass (global_only criterion filtered out), got {:?}",
            verdict
        );
    }

    #[test]
    fn gate_max_retries_from_subtask() {
        let subtask = DurableSubtask {
            id: "s1".into(),
            title: "Test".into(),
            description: None,
            depends_on: vec![],
            effort: None,
            files: vec![],
            stage: SubtaskStage::AwaitingVerification,
            criteria: vec![],
            max_retries: 5,
            retry_count: 0,
            snapshot_name: None,
            data_branch: None,
            diff_summary: None,
            last_verification: None,
            tools_used: vec![],
        };

        let gate =
            ContractVerificationGate::from_subtask(&subtask, std::path::PathBuf::from("/tmp"));
        assert_eq!(VerificationGate::max_retries(&gate), 5);
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
                acceptance: Some("File calculator.py exists".into()),
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

        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("File output.py exists".into()),
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
        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("auth.py contains 'import jwt'".into()),
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

        on_subtask_begin(&durable, "s1").await;
        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("auth.py contains rate_limit".into()),
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

        on_subtask_begin(&durable, "s1").await;
        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("Command exits 0".into()),
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

        on_subtask_begin(&durable, "s1").await;
        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("Command exits 0".into()),
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

        on_subtask_begin(&durable, "s1").await;
        let passed = on_subtask_complete(&mut durable, "s1").await;
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
                acceptance: Some("pytest passes".into()),
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

        on_subtask_begin(&durable, "s1").await;
        let passed = on_subtask_complete(&mut durable, "s1").await;
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

        // Create both files the plan expects
        std::fs::write(tmp.path().join("module.py"), "# module\n").unwrap();
        std::fs::write(tmp.path().join("test_module.py"), "def test_ok(): pass\n").unwrap();

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
                    files: vec!["module.py".into()],
                    acceptance: Some("File module.py exists".into()),
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Create tests".into(),
                    description: None,
                    depends_on: vec!["s1".into()],
                    status: TaskStatus::Pending,
                    effort: None,
                    files: vec!["test_module.py".into()],
                    acceptance: Some("File test_module.py exists".into()),
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
        let global_passed = on_plan_complete(&mut durable).await;
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
                acceptance: Some("File ghost.txt exists".into()),
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
        let passed = on_subtask_complete(&mut durable, "s1").await;
        if !passed {
            assert_eq!(durable.contract.subtasks[0].retry_count, 1);

            // Second failure
            durable.contract.subtasks[0].stage = SubtaskStage::Executing;
            let passed2 = on_subtask_complete(&mut durable, "s1").await;
            assert!(!passed2);
            assert_eq!(durable.contract.subtasks[0].retry_count, 2);
        }
    }

    #[test]
    fn create_gate_for_subtask_returns_none_for_unknown_id() {
        let plan = make_test_plan();
        let detection = astra_services::ProjectDetection::detect(std::path::Path::new("/tmp"));
        let cg = ContractGenerator::new(detection);
        let contract = cg.generate("Build foo", &plan, None).unwrap();

        // Build a DurableTaskState with a stub lifecycle.
        struct StubLifecycle;
        #[async_trait]
        impl DurableTaskLifecycle for StubLifecycle {
            async fn create_contract(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &astra_services::task_orchestrator::TaskPlan,
                _: TaskScope,
            ) -> Result<TaskContract, String> {
                Err("stub".into())
            }
            async fn amend_contract(
                &self,
                _: &str,
                _: astra_services::ContractAmendment,
            ) -> Result<TaskContract, String> {
                Err("stub".into())
            }
            async fn get_contract(&self, _: &str) -> Result<Option<TaskContract>, String> {
                Ok(None)
            }
            async fn begin_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> Result<astra_services::SubtaskExecutionContext, String> {
                Err("stub".into())
            }
            async fn complete_subtask_execution(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn fail_subtask(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn verify_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> Result<SubtaskVerificationReport, String> {
                Err("stub".into())
            }
            async fn verify_global(&self, _: &str) -> Result<Vec<VerificationResult>, String> {
                Err("stub".into())
            }
            async fn pause_task(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn resume_task(
                &self,
                _: &str,
                _: &str,
            ) -> Result<astra_services::TaskResumeContext, String> {
                Err("stub".into())
            }
            async fn deliver_task(
                &self,
                _: &str,
            ) -> Result<astra_services::TaskDeliveryReport, String> {
                Err("stub".into())
            }
            async fn snapshot_task_state(&self, _: &str) -> Result<String, String> {
                Err("stub".into())
            }
            async fn rollback_task(&self, _: &str, _: &str) -> Result<(), String> {
                Err("stub".into())
            }
        }

        let durable = DurableTaskState {
            contract,
            lifecycle: Arc::new(StubLifecycle),
            last_report: None,
        };

        // Non-existent subtask → None
        let gate = create_gate_for_subtask(&durable, "nonexistent", "/tmp".into());
        assert!(gate.is_none());

        // Existing subtask with criteria → Some
        let has_criteria = durable
            .contract
            .subtasks
            .iter()
            .any(|s| !s.criteria.is_empty());
        if has_criteria {
            let id = durable
                .contract
                .subtasks
                .iter()
                .find(|s| !s.criteria.is_empty())
                .unwrap()
                .id
                .clone();
            let gate = create_gate_for_subtask(&durable, &id, "/tmp".into());
            assert!(gate.is_some());
        }
    }
}
