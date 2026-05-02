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

/// Build a compact JSON summary of the suite for the LLM prompt.
/// Strips large fields (text, stderr, session) to stay within context.
fn build_summary_payload(report: &SuiteReport) -> serde_json::Value {
    let runs: Vec<serde_json::Value> = report
        .runs
        .iter()
        .map(|r| {
            let judger_score = r.criteria.iter().find_map(|c| c.score).unwrap_or(-1.0);
            serde_json::json!({
                "case": r.case_name,
                "model": r.model,
                "passed": r.passed,
                "capability": r.capability.as_ref().map(|c| c.to_string()),
                "difficulty": r.difficulty,
                "weight": r.weight,
                "exit_code": r.outcome.exit_code,
                "tokens": r.outcome.prompt_tokens + r.outcome.completion_tokens,
                "duration_ms": r.outcome.duration_ms,
                "turn_rounds": r.outcome.turn_rounds,
                "tool_calls": r.outcome.tool_calls_count,
                "judger_score": judger_score,
                "failure_class": r.failure_class.as_ref().map(|c| c.to_string()),
            })
        })
        .collect();
    serde_json::json!({
        "total": report.total(),
        "passed": report.passed(),
        "failed": report.failed(),
        "wall_time_ms": report.wall_time_ms,
        "runs": runs,
    })
}

fn build_summarizer_prompt(payload: &serde_json::Value) -> String {
    format!(
        "You are an expert evaluator analyzing agent benchmark results.\n\
         \n\
         Below is a JSON summary of a test suite run. Each entry represents one \
         (case, model) pair with its pass/fail status, token usage, duration, \
         turn rounds, tool calls, judger score (-1 if not scored), capability \
         category, difficulty level (1=easy, 5=hard), and failure classification.\n\
         \n\
         ```json\n{}\n```\n\
         \n\
         Produce a concise analysis (3-6 sentences) covering:\n\
         1. Which model(s) performed best overall and why\n\
         2. Where each model struggles (capability gaps, difficulty cliff)\n\
         3. Efficiency differences (token usage, duration, turn count for equivalent tasks)\n\
         4. Any notable patterns (e.g. all models fail the same case → case issue vs model issue)\n\
         \n\
         Be specific — cite case names, numbers, and concrete comparisons. \
         Do not repeat the raw data. End with one actionable recommendation.",
        serde_json::to_string_pretty(payload).unwrap_or_default()
    )
}

/// Run the summarizer by invoking the astra CLI. Returns the summary text.
pub async fn summarize(
    astra_bin: &Path,
    model: &str,
    report: &SuiteReport,
    timeout_seconds: u64,
) -> Result<String, String> {
    use tokio::process::Command;

    let payload = build_summary_payload(report);
    let prompt = build_summarizer_prompt(&payload);

    let child = Command::new(astra_bin)
        .arg("chat")
        .arg("-m")
        .arg(&prompt)
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
    let text = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| stdout.trim().to_string());

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
    use crate::report::{CaseRunReport, SuiteReport};
    use crate::runner::RunOutcome;

    #[test]
    fn summary_payload_is_compact_json() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "c1".into(),
                model: "m".into(),
                passed: true,
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
        // Must NOT contain large fields
        assert!(run.get("text").is_none());
        assert!(run.get("stderr").is_none());
        assert!(run.get("session").is_none());
    }

    #[test]
    fn summarizer_prompt_contains_analysis_instructions() {
        let payload = serde_json::json!({"total": 1, "passed": 1, "runs": []});
        let prompt = build_summarizer_prompt(&payload);
        assert!(prompt.contains("expert evaluator"));
        assert!(prompt.contains("capability gaps"));
        assert!(prompt.contains("actionable recommendation"));
    }
}
