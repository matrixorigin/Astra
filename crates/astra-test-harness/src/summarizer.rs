//! Optional LLM-powered post-run summary.
//!
//! After all cases finish, the harness can feed a compact representation
//! of the suite results to a summarizer model. The model produces a
//! human-readable paragraph highlighting strengths, weaknesses, and
//! notable patterns across models / capabilities / difficulty levels.
//!
//! Enabled via `--summarize` (uses the judger model) or
//! `--summarize-model <MODEL>`.

use std::path::Path;
use std::time::Duration;

use crate::report::SuiteReport;

/// Build a rich summary payload for the LLM. Includes criteria details,
/// severity, warnings, and truncated output for failed cases.
fn build_summary_payload(report: &SuiteReport) -> serde_json::Value {
    let runs: Vec<serde_json::Value> = report
        .runs
        .iter()
        .map(|r| {
            let criteria: Vec<serde_json::Value> = r
                .criteria
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "passed": c.passed,
                        "severity": format!("{:?}", c.severity),
                        "detail": &c.detail,
                        "score": c.score,
                    })
                })
                .collect();
            let output_preview: String = r.outcome.text.chars().take(300).collect();
            let attempts: Vec<serde_json::Value> = r
                .attempts
                .iter()
                .map(|attempt| {
                    let outcome = &attempt.outcome;
                    let stderr_preview: String = outcome.stderr.chars().take(300).collect();
                    serde_json::json!({
                        "attempt_index": attempt.attempt_index,
                        "exit_code": outcome.exit_code,
                        "final_state": outcome.final_state,
                        "session_id": outcome.session_id,
                        "run_id": outcome.run_id,
                        "tokens": outcome.prompt_tokens + outcome.completion_tokens,
                        "duration_ms": outcome.duration_ms,
                        "tool_calls": outcome.tool_calls_count,
                        "tools_used": outcome.tools_used,
                        "stderr_preview": stderr_preview,
                    })
                })
                .collect();
            serde_json::json!({
                "case": r.case_name,
                "model": &r.model,
                "status": r.status,
                "passed": r.is_passed(),
                "unavailable": r.is_unavailable(),
                "cancelled": r.is_cancelled(),
                "has_warnings": r.has_warnings,
                "capability": r.capability.as_ref().map(|c| c.to_string()),
                "difficulty": r.difficulty,
                "exit_code": r.outcome.exit_code,
                "tokens": r.outcome.prompt_tokens + r.outcome.completion_tokens,
                "duration_ms": r.outcome.duration_ms,
                "turn_rounds": r.outcome.turn_rounds,
                "tool_calls": r.outcome.tool_calls_count,
                "tools_used": r.outcome.tools_used,
                "failure_class": r.failure_class.as_ref().map(|c| c.to_string()),
                "attempts": attempts,
                "criteria": criteria,
                "output_preview": output_preview,
            })
        })
        .collect();

    let models: std::collections::BTreeSet<&str> = report
        .runs
        .iter()
        .filter(|r| r.is_evidence())
        .map(|r| r.model.as_str())
        .collect();

    serde_json::json!({
        "total": report.total(),
        "passed": report.passed(),
        "failed": report.failed(),
        "cancelled": report.cancelled(),
        "unavailable": report.unavailable(),
        "warnings": report.runs.iter().filter(|r| r.has_warnings).count(),
        "wall_time_ms": report.wall_time_ms,
        "models_tested": models,
        "runs": runs,
    })
}

fn build_summarizer_prompt(payload: &serde_json::Value) -> String {
    format!(
        "You are a senior test engineer writing a diagnostic report for an agent \
         benchmark run. You must analyze the results across FIVE mandatory dimensions.\n\
         \n\
         ## Input Data\n\
         ```json\n{}\n```\n\
         \n\
         ## Output: Write a structured report with these FIVE sections. Use markdown.\n\
         \n\
         ### 1. Runtime Process Assessment\n\
         Evaluate how the test execution itself went:\n\
         - Infrastructure issues (auth failures, timeouts, rate limits)\n\
         - Cases where exit_code != 0 — is this a runtime/infra problem or an agent issue?\n\
         - Token efficiency anomalies (some models using 10x more tokens for the same task)\n\
         - Cases with has_warnings=true — what soft criteria were violated?\n\
         \n\
         ### 2. Result Summary\n\
         For each model tested:\n\
         - Pass rate, but distinguish Hard failures vs Soft warnings vs Quality scores\n\
         - Which capabilities the model handles well vs poorly\n\
         - Difficulty cliff: at what difficulty level does the model start failing?\n\
         - Token/duration efficiency compared to other models\n\
         \n\
         ### 3. Astra Agent Issues (HIGHEST SEVERITY)\n\
         Problems that indicate bugs in the astra runtime/agent itself, NOT model limitations:\n\
         - All models failing the same case → likely a case design or runtime bug\n\
         - exit_code issues that aren't model-related\n\
         - Unexpected tool call failures\n\
         - Session continuity problems in multi-turn cases\n\
         Rate each issue: CRITICAL / HIGH / MEDIUM\n\
         \n\
         ### 4. Case Design Issues\n\
         Cases that may have flawed criteria or unrealistic expectations:\n\
         - Cases where the agent output looks correct but criteria say FAIL\n\
         - Criteria that are too strict (exact string match on free-form output)\n\
         - Cases where ALL models fail — the case itself may be broken\n\
         - Missing criteria that should exist (agent did something wrong but passed)\n\
         \n\
         ### 5. Model Capability Comparison\n\
         Only for multi-model runs. For each model:\n\
         - Strengths: what it does better than others (cite specific cases)\n\
         - Weaknesses: capability gaps (cite specific failures)\n\
         - Efficiency: token/duration/turns comparison on equivalent tasks\n\
         - Recommendation: what each model is best suited for\n\
         \n\
         ## Rules\n\
         - Be SPECIFIC: cite case names, exact numbers, concrete comparisons\n\
         - Distinguish between model limitation vs infrastructure issue vs case bug\n\
         - If a model's output_preview shows correct work but criteria say FAIL, \
           flag it as a potential case issue, NOT a model failure\n\
         - Use severity tags: 🔴 CRITICAL, 🟡 WARNING, 🟢 OK\n\
         - Keep each section to 3-5 bullet points maximum",
        serde_json::to_string_pretty(payload).unwrap_or_default()
    )
}

/// Run the summarizer by invoking the astra CLI. Returns the summary text.
pub async fn summarize(
    astra_bin: &Path,
    profile: Option<&str>,
    model: &str,
    report: &SuiteReport,
    timeout_seconds: u64,
) -> Result<String, String> {
    use tokio::process::Command;

    let payload = build_summary_payload(report);
    let prompt = build_summarizer_prompt(&payload);

    let mut command = Command::new(astra_bin);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    let child = command
        .arg("chat")
        .arg("-m")
        .arg(&prompt)
        .arg("--no-resume")
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("--quiet")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn summarizer: {e}"))?;

    let timeout = Duration::from_secs(timeout_seconds);
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("summarizer timed out after {timeout_seconds}s"))?
        .map_err(|e| format!("summarizer wait: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "summarizer exited {}: {}",
            output.status,
            stderr.chars().take(500).collect::<String>()
        ));
    }
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("summarizer stdout is not valid JSON: {error}"))?;
    let text = envelope
        .get("text")
        .and_then(|value| value.as_str())
        .ok_or("summarizer stdout missing string 'text' field")?
        .to_string();

    if text.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "summarizer produced empty output (exit={}, stderr={})",
            output.status.code().unwrap_or(-1),
            stderr.chars().take(500).collect::<String>()
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{AttemptRecord, CaseRunReport, CaseRunStatus, SuiteReport};
    use crate::runner::RunOutcome;

    #[test]
    fn summary_payload_is_compact_json() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "c1".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: Some(crate::case::Capability::ToolUse),
                weight: 2.0,
                difficulty: Some(3),
                outcome: {
                    let mut o = RunOutcome::new("m");
                    o.prompt_tokens = 100;
                    o.completion_tokens = 50;
                    o.duration_ms = 1000;
                    o.turn_rounds = 2;
                    o.tool_calls_count = 5;
                    o
                },
                criteria: vec![],
                steps: vec![],
                attempts: Vec::new(),
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
                failure_class: None,
                has_warnings: false,
            }],
            ..Default::default()
        };
        let payload = build_summary_payload(&r);
        assert_eq!(payload["total"], 1);
        assert_eq!(payload["passed"], 1);
        let run = &payload["runs"][0];
        assert_eq!(run["case"], "c1");
        assert_eq!(run["tokens"], 150);
        assert_eq!(run["difficulty"], 3);
        assert_eq!(run["capability"], "tool_use");
        assert!(run.get("criteria").is_some());
        assert!(run.get("output_preview").is_some());
        assert!(run.get("has_warnings").is_some());
        // Must NOT contain large fields
        assert!(run.get("stderr").is_none());
        assert!(run.get("session").is_none());
    }

    #[test]
    fn summary_payload_excludes_cancelled_only_models_from_tested_models() {
        let mut cancelled = CaseRunReport {
            case_name: "c1".into(),
            model: "m".into(),
            status: CaseRunStatus::Cancelled,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            has_warnings: false,
        };
        cancelled.outcome.text = "not executed".into();
        let payload = build_summary_payload(&SuiteReport {
            runs: vec![cancelled],
            ..Default::default()
        });
        assert_eq!(payload["cancelled"], 1);
        assert_eq!(payload["models_tested"].as_array().unwrap().len(), 0);
        assert_eq!(payload["runs"][0]["cancelled"], true);
    }

    #[test]
    fn summary_attempts_are_bounded_projections() {
        let mut first = RunOutcome::new("m");
        first.text = "x".repeat(200_000);
        first.stderr = "y".repeat(200_000);
        let report = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "retry".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: RunOutcome::new("m"),
                criteria: vec![],
                steps: vec![],
                attempts: vec![AttemptRecord {
                    attempt_index: 0,
                    outcome: first,
                }],
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
                failure_class: None,
                has_warnings: true,
            }],
            ..Default::default()
        };
        let payload = build_summary_payload(&report);
        let attempt = &payload["runs"][0]["attempts"][0];
        assert!(attempt.get("text").is_none());
        assert!(attempt.get("stderr").is_none());
        assert!(attempt["stderr_preview"].as_str().unwrap().len() <= 300);
        assert!(serde_json::to_vec(&payload).unwrap().len() < 10_000);
    }

    #[test]
    fn summarizer_prompt_contains_five_dimensions() {
        let payload = serde_json::json!({"total": 1, "passed": 1, "runs": []});
        let prompt = build_summarizer_prompt(&payload);
        assert!(prompt.contains("Runtime Process Assessment"));
        assert!(prompt.contains("Result Summary"));
        assert!(prompt.contains("Astra Agent Issues"));
        assert!(prompt.contains("Case Design Issues"));
        assert!(prompt.contains("Model Capability Comparison"));
    }

    #[tokio::test]
    async fn summarizer_rejects_text_emitted_by_failed_subprocess() {
        use crate::test_support::write_executable_shim;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            "#!/bin/sh\necho '{\"text\":\"fabricated summary\"}'\necho 'model failed' 1>&2\nexit 17\n",
        )
        .expect("write shim");

        let error = summarize(&shim, None, "summary-model", &SuiteReport::default(), 5)
            .await
            .expect_err("non-zero summarizer must not become a successful narrative");
        assert!(error.contains("summarizer exited"), "{error}");
        assert!(error.contains("model failed"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn summarizer_rejects_raw_text_from_successful_subprocess() {
        use crate::test_support::write_executable_shim;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(&shim, "#!/bin/sh\nprintf '%s\\n' 'raw summary'\n")
            .expect("write shim");

        let error = summarize(&shim, None, "summary-model", &SuiteReport::default(), 5)
            .await
            .expect_err("raw stdout is not the typed summary envelope");
        assert!(error.contains("not valid JSON"), "{error}");
    }
}
