//! Report rendering.
//!
//! After all cases × models have run, the harness folds every
//! result into one `SuiteReport` and emits it in one of two formats:
//!
//! - `text` — human-scannable, grouped by case, colored PASS/FAIL
//!   markers, one line per criterion. Default.
//! - `json` — machine-readable dump used by CI / dashboards. Shape
//!   mirrors the in-memory struct so downstream consumers can
//!   deserialize without a schema doc.

use serde::{Deserialize, Serialize};

use crate::criteria::CriterionResult;
use crate::runner::RunOutcome;
use crate::session_capture::SessionCapture;

/// One (case, model) pair's full result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRunReport {
    pub case_name: String,
    pub model: String,
    pub passed: bool,
    pub outcome: RunOutcome,
    pub criteria: Vec<CriterionResult>,
    /// Optional session journal dump — only present when
    /// `debug_log: true` on the case or `--capture-session` on the CLI.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session: Option<SessionCapture>,
    /// Shell command a developer can paste to re-run the case in a
    /// terminal. Surfaced in text reports after FAIL so debugging is
    /// a copy-paste away. `None` in unit tests with fake executors.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reproducer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SuiteReport {
    pub runs: Vec<CaseRunReport>,
}

impl SuiteReport {
    pub fn total(&self) -> usize {
        self.runs.len()
    }
    pub fn passed(&self) -> usize {
        self.runs.iter().filter(|r| r.passed).count()
    }
    pub fn failed(&self) -> usize {
        self.total() - self.passed()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "text" | "txt" | "human" => Ok(Format::Text),
            "json" => Ok(Format::Json),
            other => Err(format!("unknown format {other:?} (expected text|json)")),
        }
    }
}

/// Render a report into a String. Pure function — no side effects —
/// so tests can assert on the output without touching stdout.
pub fn render(report: &SuiteReport, fmt: Format, verbose: bool) -> String {
    match fmt {
        Format::Json => serde_json::to_string_pretty(report).unwrap_or_default(),
        Format::Text => render_text(report, verbose),
    }
}

fn render_text(report: &SuiteReport, verbose: bool) -> String {
    let mut s = String::new();
    s.push_str("=== astra-test suite report ===\n");
    s.push_str(&format!(
        "total={} passed={} failed={}\n\n",
        report.total(),
        report.passed(),
        report.failed()
    ));
    for run in &report.runs {
        let marker = if run.passed { "PASS" } else { "FAIL" };
        s.push_str(&format!(
            "[{marker}] case={} model={} exit={} tools={} dur={}ms\n",
            run.case_name,
            run.model,
            run.outcome.exit_code,
            run.outcome.tool_calls_count,
            run.outcome.duration_ms
        ));
        for c in &run.criteria {
            let m = if c.passed { " ok " } else { "FAIL" };
            s.push_str(&format!("    [{m}] {}\n", c.detail));
        }
        if verbose || !run.passed {
            if !run.outcome.text.is_empty() {
                s.push_str(&format!(
                    "    text: {}\n",
                    truncate(&run.outcome.text, 500)
                ));
            }
            if !run.outcome.stderr.is_empty() {
                s.push_str(&format!(
                    "    stderr: {}\n",
                    truncate(&run.outcome.stderr, 500)
                ));
            }
        }
        if let Some(cap) = &run.session {
            s.push_str(&format!(
                "    session: id={} events={} skipped={} tools_from_journal={:?}\n",
                cap.session_id,
                cap.events.len(),
                cap.skipped_lines,
                cap.tools_invoked()
            ));
        }
        // Diagnostic hints on FAIL — copy-paste debugging commands.
        if !run.passed {
            if let Some(id) = run.outcome.session_id.as_deref() {
                s.push_str(&format!(
                    "    journal: ~/.astra/sessions/{id}.jsonl\n"
                ));
                s.push_str(&format!(
                    "    hint:    jq -r 'select(.type==\"tool_invocation\") | .metadata.tool_name' ~/.astra/sessions/{id}.jsonl\n"
                ));
            }
            if let Some(repro) = run.reproducer.as_deref() {
                s.push_str(&format!("    rerun:   {repro}\n"));
            }
        }
        s.push('\n');
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}… ({} chars total)", s.chars().count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criteria::Criterion;

    fn mk_outcome() -> RunOutcome {
        RunOutcome {
            model: "m".into(),
            exit_code: 0,
            text: "hello".into(),
            stderr: String::new(),
            session_id: Some("sess".into()),
            run_id: None,
            tool_calls_count: 1,
            tools_used: vec!["Read".into()],
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: 42,
        }
    }

    fn mk_report_passed() -> SuiteReport {
        SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "c1".into(),
                model: "m".into(),
                passed: true,
                outcome: mk_outcome(),
                criteria: vec![CriterionResult {
                    criterion: Criterion::ToolCalled {
                        name: "Read".into(),
                    },
                    passed: true,
                    detail: "tool Read was called".into(),
                    score: None,
                }],
                session: None,
                reproducer: None,
            }],
        }
    }

    #[test]
    fn text_report_has_pass_marker_and_counts() {
        let r = mk_report_passed();
        let out = render(&r, Format::Text, false);
        assert!(out.contains("[PASS]"));
        assert!(out.contains("total=1"));
        assert!(out.contains("passed=1"));
        assert!(out.contains("failed=0"));
    }

    #[test]
    fn text_report_shows_stderr_and_text_on_fail() {
        let mut r = mk_report_passed();
        r.runs[0].passed = false;
        r.runs[0].outcome.stderr = "boom".into();
        let out = render(&r, Format::Text, false);
        assert!(out.contains("[FAIL]"));
        assert!(out.contains("text: hello"));
        assert!(out.contains("stderr: boom"));
    }

    #[test]
    fn json_report_roundtrips() {
        let r = mk_report_passed();
        let out = render(&r, Format::Json, false);
        let parsed: SuiteReport = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.total(), 1);
        assert_eq!(parsed.passed(), 1);
    }

    #[test]
    fn format_from_str_accepts_common_aliases() {
        use std::str::FromStr;
        assert_eq!(Format::from_str("text").unwrap(), Format::Text);
        assert_eq!(Format::from_str("TXT").unwrap(), Format::Text);
        assert_eq!(Format::from_str("json").unwrap(), Format::Json);
        assert!(Format::from_str("yaml").is_err());
    }

    #[test]
    fn text_report_emits_diag_hints_on_fail() {
        let mut r = mk_report_passed();
        r.runs[0].passed = false;
        r.runs[0].reproducer =
            Some("/path/to/astra chat -m 'say ok' --model m --json -y".into());
        let out = render(&r, Format::Text, false);
        assert!(out.contains("journal: ~/.astra/sessions/sess.jsonl"));
        assert!(out.contains("jq "));
        assert!(out.contains("rerun:"));
        assert!(out.contains("/path/to/astra chat"));
    }

    #[test]
    fn text_report_skips_diag_hints_on_pass() {
        let mut r = mk_report_passed();
        r.runs[0].reproducer = Some("astra chat -m 'ok' --model m --json -y".into());
        let out = render(&r, Format::Text, false);
        assert!(!out.contains("journal:"));
        assert!(!out.contains("rerun:"));
    }

    #[test]
    fn suite_report_counters() {
        let mut r = mk_report_passed();
        r.runs.push(CaseRunReport {
            case_name: "c2".into(),
            model: "m".into(),
            passed: false,
            outcome: mk_outcome(),
            criteria: vec![],
            session: None,
            reproducer: None,
        });
        assert_eq!(r.total(), 2);
        assert_eq!(r.passed(), 1);
        assert_eq!(r.failed(), 1);
    }
}
