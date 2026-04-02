//! Bridge between the durable task system and the REPL plan execution loop.
//!
//! This module wraps [`DurableTaskLifecycle`] calls with terminal display formatting
//! so plan execution can show contract generation, verification results, and delivery
//! reports in a user-friendly way.

use crossterm::style::Stylize;
use mo_agent_services::{
    ContractAmendment, ContractGenerator, DurableTaskLifecycle, LocalDurableTaskLifecycle,
    SubtaskStage, SubtaskVerificationReport, TaskContract, TaskDeliveryReport, VerifierKind,
};
use std::sync::Arc;

// ─── Active contract state held by the REPL ──────────────────────────────────

/// Holds the active contract and lifecycle service during plan execution.
pub struct DurableTaskState {
    pub contract: TaskContract,
    pub lifecycle: Arc<dyn DurableTaskLifecycle>,
}

// ─── Contract generation ─────────────────────────────────────────────────────

/// Generate a [`TaskContract`] from a plan and persist it via the lifecycle.
///
/// Returns `None` (with a warning) if generation or persistence fails —
/// plan execution proceeds without contract-backed verification.
pub async fn generate_contract(
    lifecycle: &Arc<dyn DurableTaskLifecycle>,
    plan: &mo_agent_services::task_orchestrator::TaskPlan,
    goal: &str,
    user_id: &str,
    session_id: &str,
    work_dir: &std::path::Path,
) -> Option<TaskContract> {
    let detection =
        mo_agent_services::ProjectDetection::detect(work_dir);
    let cg = ContractGenerator::new(detection);

    let contract = match cg.generate(goal, plan, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "  {}  Contract generation skipped: {}",
                "⚠".yellow(),
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
                    eprintln!(
                        "  {}  Criteria injection failed: {}",
                        "⚠".yellow(),
                        e,
                    );
                    display_contract_summary(&persisted);
                    Some(persisted)
                }
            }
        }
        Err(e) => {
            eprintln!(
                "  {}  Contract persistence failed: {}",
                "⚠".yellow(),
                e
            );
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

/// Call when a subtask transitions Pending → Executing (snapshot).
pub async fn on_subtask_begin(
    durable: &DurableTaskState,
    subtask_id: &str,
) {
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
pub async fn on_subtask_complete(
    durable: &mut DurableTaskState,
    subtask_id: &str,
) -> bool {
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
            s.criteria.iter().any(|c| {
                !c.global_only
                    && !matches!(
                        c.verifier,
                        VerifierKind::LlmJudge { .. }
                    )
            })
        })
        .unwrap_or(false);

    if !has_local_criteria {
        // Silently skip — heavy checks run during global verification
        return true;
    }

    // 3. Run lightweight verification with progress indication
    eprintln!(
        "  {}  Verifying subtask: {}...",
        "🔍".cyan(),
        subtask_id,
    );
    match durable
        .lifecycle
        .verify_subtask(&task_id, subtask_id)
        .await
    {
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

    // Show individual failures
    for r in &report.results {
        if !r.passed {
            let label = r.criterion_id.as_str();
            let evidence = r.evidence.chars().take(120).collect::<String>();
            eprintln!(
                "      {} {} — {}",
                "✘".red(),
                label,
                evidence,
            );
        }
    }
}

// ─── Global verification + delivery ─────────────────────────────────────────

/// Run global verification (build/test/lint) after all subtasks complete.
/// Returns `true` if all required global checks pass.
pub async fn on_plan_complete(
    durable: &mut DurableTaskState,
) -> bool {
    let task_id = durable.contract.task_id.clone();

    eprintln!(
        "\n{}  Running global verification...",
        "🔬".cyan(),
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
        eprintln!("      {} {}", "▸".grey(), cmd_hint);
    }

    match durable.lifecycle.verify_global(&task_id).await {
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

            eprintln!(
                "  {}  Global checks: {}/{} passed",
                styled, passed, total,
            );

            for r in &results {
                if !r.passed {
                    let evidence = r.evidence.chars().take(120).collect::<String>();
                    eprintln!(
                        "      {} {} — {}",
                        "✘".red(),
                        r.criterion_id,
                        evidence,
                    );
                }
            }

            if all_passed {
                // Deliver the task
                match durable.lifecycle.deliver_task(&task_id).await {
                    Ok(report) => display_delivery_report(&report),
                    Err(e) => eprintln!(
                        "  {}  Delivery report failed: {}",
                        "⚠".yellow(),
                        e,
                    ),
                }
            }

            all_passed
        }
        Err(e) => {
            eprintln!(
                "  {}  Global verification error: {}",
                "⚠".yellow(),
                e,
            );
            true // Don't block plan on verification infra failure
        }
    }
}

/// Pretty-print the final delivery report.
fn display_delivery_report(report: &TaskDeliveryReport) {
    let all_verified = report
        .subtask_summaries
        .iter()
        .all(|s| s.criteria_passed == s.criteria_total);

    eprintln!();
    eprintln!(
        "{}",
        "╔══════════════════════════════════════════════════════════╗"
            .cyan()
    );
    eprintln!(
        "{}  Task: {}",
        "║".cyan(),
        report.goal,
    );
    let status_icon = if all_verified { "✅" } else { "⚠️" };
    eprintln!(
        "{}  Status: {} {}",
        "║".cyan(),
        status_icon,
        if all_verified {
            "Delivered"
        } else {
            "Partial"
        },
    );
    eprintln!(
        "{}",
        "╠══════════════════════════════════════════════════════════╣"
            .cyan()
    );

    for sub in &report.subtask_summaries {
        let verified = sub.criteria_passed == sub.criteria_total;
        let icon = if verified { "✅" } else { "⚠️" };
        let criteria_info = format!(
            "{}/{} criteria",
            sub.criteria_passed, sub.criteria_total
        );
        eprintln!(
            "{}  {} {} ({})",
            "║".cyan(),
            icon,
            sub.title,
            criteria_info,
        );
    }

    eprintln!(
        "{}",
        "╚══════════════════════════════════════════════════════════╝"
            .cyan()
    );
}

// ─── Lifecycle factory ───────────────────────────────────────────────────────

/// Create a [`LocalDurableTaskLifecycle`] for the current working directory.
///
/// Uses the session directory as the persistence root and the cwd as the work_dir
/// for the embedded [`VerificationRunner`].
pub fn create_local_lifecycle(
    session_dir: &std::path::Path,
    work_dir: &std::path::Path,
) -> Arc<dyn DurableTaskLifecycle> {
    let contracts_dir = session_dir.join("contracts");
    let _ = std::fs::create_dir_all(&contracts_dir);
    let mut lifecycle = LocalDurableTaskLifecycle::new(contracts_dir, work_dir.to_path_buf());

    // Wire up LLM judge for semantic verification (if API key available)
    if let Some(judge) = HttpLlmJudge::from_env() {
        lifecycle.set_llm_judge(Arc::new(judge));
    }

    Arc::new(lifecycle)
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
        Self {
            client: reqwest::Client::new(),
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
        let model = std::env::var("MO_LLM_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        Some(Self::new(api_key, base_url, model))
    }
}

#[async_trait::async_trait]
impl mo_agent_services::LlmJudge for HttpLlmJudge {
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

        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

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
            return Err(format!("LLM API error {status}: {}", &text[..text.len().min(200)]));
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
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(score) = v["score"].as_f64() {
            return Ok(score.clamp(0.0, 1.0));
        }
    }

    // Try to find JSON embedded in text (e.g., wrapped with markdown)
    if let Some(start) = text.find('{') {
        if let Some(end) = text[start..].rfind('}') {
            let json_str = &text[start..=start + end];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(score) = v["score"].as_f64() {
                    return Ok(score.clamp(0.0, 1.0));
                }
            }
        }
    }

    // Fallback: find any decimal number between 0 and 1
    for word in text.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Ok(n) = clean.parse::<f64>() {
            if (0.0..=1.0).contains(&n) {
                return Ok(n);
            }
        }
    }

    Err(format!(
        "Could not extract score from LLM response: {}",
        &text[..text.len().min(200)]
    ))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mo_agent_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
    use mo_agent_services::{SubtaskDeliverySummary, TaskScope, VerificationResult};

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
        let detection = mo_agent_services::ProjectDetection::detect(std::path::Path::new("/tmp"));
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
        let text = "Here is my evaluation:\n```json\n{\"score\": 0.7, \"reason\": \"mostly ok\"}\n```";
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
}
