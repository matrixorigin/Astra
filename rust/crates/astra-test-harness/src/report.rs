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
use crate::digest::DigestArtifact;
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
    /// Aggregated journal digest — populated on FAIL when a
    /// DigestCollector is configured and the outcome carries a
    /// session_id. Embeds turn counts, tokens, tool_calls, errors
    /// so a reviewer sees the whole shape of the run without
    /// running `astra journal digest` by hand.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub digest: Option<DigestArtifact>,
    /// Error from the digest collector when it was attempted but
    /// failed (bin missing, session not yet flushed, JSON parse
    /// error). Kept as a string so the report surfaces the reason
    /// without hiding it inside the case FAIL.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub digest_error: Option<String>,
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
            // FAIL or --verbose: dump the untruncated diagnostic if
            // the criterion carries one (judger quorum votes, etc.).
            // Indented block so it's visually nested under the fail.
            if (!c.passed || verbose)
                && let Some(full) = c.full_detail.as_deref()
                && full != c.detail
            {
                for line in full.lines() {
                    s.push_str("        ");
                    s.push_str(line);
                    s.push('\n');
                }
            }
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
                // Session ids should be UUIDs or simple slugs. If the
                // runtime ever returns something richer, we refuse to
                // splice it into shell hints — a malicious/stale id
                // with shell metachars could make the copy-paste hint
                // a remote-execution vector for an unwary developer.
                if is_safe_session_id(id) {
                    s.push_str(&format!(
                        "    journal: ~/.astra/sessions/{id}.jsonl\n"
                    ));
                    s.push_str(&format!(
                        "    hint:    jq -r '.tool_calls[]?.name' ~/.astra/sessions/{id}.jsonl\n"
                    ));
                    s.push_str(&format!(
                        "    hint-steps: jq -r '.event_type + \" \" + .payload.tool_name' ~/.astra/sessions/{id}/step_events.jsonl 2>/dev/null\n"
                    ));
                } else {
                    // Report the anomaly so the reviewer sees SOMETHING,
                    // just never in a shell-splice position.
                    s.push_str(&format!(
                        "    journal: (session_id has unexpected characters: {:?}; hint suppressed)\n",
                        truncate(id, 80)
                    ));
                }
            }
            if let Some(repro) = run.reproducer.as_deref() {
                s.push_str(&format!("    rerun:   {repro}\n"));
            }
            if let Some(d) = &run.digest {
                // Render compact summary lines from the digest JSON
                // rather than dumping the whole blob. Reviewers get
                // the numbers they need to triage without scrolling
                // through structured data; full blob is in JSON format.
                s.push_str("    digest:\n");
                render_digest_summary(&d.json, &mut s);
            }
            if let Some(err) = run.digest_error.as_deref() {
                s.push_str(&format!("    digest_error: {err}\n"));
            }
        }
        s.push('\n');
    }
    s
}

/// Extract scannable lines from a `astra journal digest --focus summary`
/// JSON blob. Defensive about missing fields — a schema change should
/// shrink the rendered block, not panic the whole report.
fn render_digest_summary(json: &serde_json::Value, out: &mut String) {
    let aggr = json.get("aggregates");
    if let Some(a) = aggr {
        let get_u = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        out.push_str(&format!(
            "      turns={} tool_calls={} tool_failures={} errors={} compacts={} stalls={}\n",
            get_u("turns"),
            get_u("tool_calls"),
            get_u("tool_failures"),
            get_u("errors"),
            get_u("compacts"),
            get_u("stalls"),
        ));
        out.push_str(&format!(
            "      tokens_in={} tokens_out={} duration_ms={}\n",
            get_u("tokens_in"),
            get_u("tokens_out"),
            get_u("duration_ms"),
        ));
    }
    if let Some(avg) = json.get("averages_per_turn") {
        let get_f = |k: &str| avg.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        out.push_str(&format!(
            "      avg_tokens_in={:.1} avg_tokens_out={:.1} avg_duration_ms={:.1}\n",
            get_f("tokens_in"),
            get_f("tokens_out"),
            get_f("duration_ms"),
        ));
    }
    // Point the reviewer at the full digest — if and only if the
    // session_id is a recognized shape. See `is_safe_session_id`
    // for why: any id with shell metachars would turn a friendly
    // copy-paste into an injection vector.
    if let Some(id) = json.get("session_id").and_then(|v| v.as_str())
        && is_safe_session_id(id)
    {
        out.push_str(&format!(
            "      full:  astra journal digest {id}\n"
        ));
    }
}

/// Whitelist for session-id characters. Strict on purpose: a session
/// id spliced into a `jq` / shell command string must not carry
/// anything that a shell could interpret. Accepts `[A-Za-z0-9_-]`
/// plus `.` (already present in some legacy ids). Everything else
/// triggers the caller to suppress the shell-splicing hint.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
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
                    full_detail: None,
                    score: None,
                }],
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
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
        // Step-events layout hint is offered alongside the legacy
        // jsonl-file hint so post-G3 runs (which live under
        // <id>/step_events.jsonl) point at the right file.
        assert!(out.contains("step_events.jsonl"));
        assert!(out.contains("rerun:"));
        assert!(out.contains("/path/to/astra chat"));
    }

    #[test]
    fn text_report_suppresses_hints_when_session_id_has_shell_metachars() {
        // Security regression: a session_id carrying `;` / `$` /
        // backticks must never be spliced into a shell-coloured
        // hint. The report falls back to a diagnostic note naming
        // the id rather than emitting the hint.
        for injection in [
            "sess; rm -rf ~",
            "sess $(rm -rf ~)",
            "sess`rm -rf ~`",
            "sess|evil",
            "sess'; echo pwned",
        ] {
            let mut r = mk_report_passed();
            r.runs[0].passed = false;
            r.runs[0].outcome.session_id = Some(injection.to_string());
            let out = render(&r, Format::Text, false);
            assert!(
                !out.contains(&format!("jq -r '.tool_calls[]?.name' ~/.astra/sessions/{injection}")),
                "jq hint must NOT splice the suspicious id: {out}"
            );
            assert!(
                out.contains("unexpected characters"),
                "diagnostic note must surface: {out}"
            );
        }
    }

    #[test]
    fn is_safe_session_id_accepts_uuid_and_slug_shapes() {
        assert!(is_safe_session_id("8e1f524e-b2e8-4d35-a992-27a5ff200c9f"));
        assert!(is_safe_session_id("sess_abc"));
        assert!(is_safe_session_id("00000000-0000-0000-0000-000000000129"));
        assert!(is_safe_session_id("abc.def"));
    }

    #[test]
    fn is_safe_session_id_rejects_shell_metachars_and_oversize() {
        // Shell metachar rejects.
        for bad in [
            "", "sess;rm", "sess|evil", "sess\"quote", "sess'quote",
            "sess`cmd`", "sess$(cmd)", "sess>file", "sess<file",
            "sess\nline", "sess\\/", "sess space",
        ] {
            assert!(!is_safe_session_id(bad), "should reject {bad:?}");
        }
        // Length cap.
        let too_long: String = std::iter::repeat_n('a', 129).collect();
        assert!(!is_safe_session_id(&too_long));
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
    fn text_report_renders_digest_summary_on_fail() {
        use crate::digest::DigestArtifact;
        let mut r = mk_report_passed();
        r.runs[0].passed = false;
        r.runs[0].digest = Some(DigestArtifact {
            session_id: "sess".into(),
            json: serde_json::json!({
                "session_id": "sess",
                "aggregates": {
                    "turns": 3,
                    "tool_calls": 5,
                    "tool_failures": 1,
                    "errors": 0,
                    "compacts": 0,
                    "stalls": 0,
                    "tokens_in": 12000,
                    "tokens_out": 450,
                    "duration_ms": 8200,
                },
                "averages_per_turn": {
                    "tokens_in": 4000.0,
                    "tokens_out": 150.0,
                    "duration_ms": 2733.33,
                }
            }),
        });
        let out = render(&r, Format::Text, false);
        assert!(out.contains("digest:"));
        assert!(out.contains("turns=3"));
        assert!(out.contains("tool_calls=5"));
        assert!(out.contains("tokens_in=12000"));
        assert!(out.contains("avg_tokens_in=4000.0"));
        assert!(out.contains("astra journal digest sess"));
    }

    #[test]
    fn text_report_renders_digest_error_on_fail() {
        let mut r = mk_report_passed();
        r.runs[0].passed = false;
        r.runs[0].digest_error = Some("digest timeout after 15s".into());
        let out = render(&r, Format::Text, false);
        assert!(out.contains("digest_error: digest timeout after 15s"));
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
            digest: None,
            digest_error: None,
        });
        assert_eq!(r.total(), 2);
        assert_eq!(r.passed(), 1);
        assert_eq!(r.failed(), 1);
    }
}
