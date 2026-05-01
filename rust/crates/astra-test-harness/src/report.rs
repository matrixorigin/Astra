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

fn default_weight() -> f64 {
    1.0
}

/// One (case, model) pair's full result.
///
/// Serialized into `--format json` reports, so this struct is a
/// de-facto public wire format. `#[non_exhaustive]` lets us add
/// fields (new diagnostic hints, new counter buckets) without a
/// SemVer break. External consumers must use `..` when matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CaseRunReport {
    pub case_name: String,
    pub model: String,
    pub passed: bool,
    /// 0-based index when `--runs N` repeats the same case/model.
    /// Always 0 for single-run mode.
    #[serde(default)]
    pub run_index: u32,
    /// Capability dimension from case metadata.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub capability: Option<crate::case::Capability>,
    /// Scoring weight from case metadata.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Difficulty level from case metadata.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub difficulty: Option<u8>,
    pub outcome: RunOutcome,
    pub criteria: Vec<CriterionResult>,
    /// Step-level results for multi-turn cases. Empty for single-turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepResult>,
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
    /// Failure classification — populated only when `passed == false`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure_class: Option<crate::classify::FailureClass>,
}

/// Result of a single step in a multi-turn case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_index: u32,
    pub prompt: String,
    pub outcome: RunOutcome,
    pub duration_ms: u64,
    /// Criteria results for this step. Empty if the step has no criteria.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<CriterionResult>,
    /// Whether all step criteria passed.
    #[serde(default = "default_true")]
    pub passed: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SuiteReport {
    pub runs: Vec<CaseRunReport>,
    /// ISO 8601 timestamp when the suite run started.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub started_at: Option<String>,
    /// ISO 8601 timestamp when the suite run ended.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended_at: Option<String>,
    /// Real wall-clock time in milliseconds (not sum of per-case durations).
    #[serde(default)]
    pub wall_time_ms: u64,
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
        Format::Json => render_json(report),
        Format::Text => render_text(report, verbose),
    }
}

/// Serialize the report to pretty JSON. On serialize failure —
/// unreachable today because every field in `SuiteReport` and its
/// transitive types is serde-safe, but a future field addition
/// could break that invariant — return a structured error blob so
/// CI consumers parsing the output see a diagnosable failure
/// rather than a zero-byte file.
///
/// Extracted as a pub(crate) helper so tests can exercise the
/// fallback branch by going through `format_render_error` directly;
/// the branch itself is genuinely hard to trip with a real report.
pub(crate) fn render_json(report: &SuiteReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_else(|e| format_render_error(&e.to_string()))
}

/// Format the JSON-render fallback body. Separated from `render_json`
/// so a test can feed a synthetic error message in without having to
/// force `serde_json::to_string_pretty` to fail (there is no safe way
/// to construct a `SuiteReport` whose serde fails today). Callers
/// must pass a human-readable error; this helper handles quoting so
/// the result is always valid JSON.
pub(crate) fn format_render_error(reason: &str) -> String {
    // JSON string quoting: escape `\` first, then `"`, then newlines
    // which would otherwise make the emitted body invalid. Minimal
    // escape — enough that the output passes a `serde_json::from_str`
    // round-trip. Non-ASCII bytes are fine inside JSON strings.
    let escaped = reason
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("{{\n  \"error\": \"SuiteReport JSON render failed: {escaped}\"\n}}")
}

fn render_text(report: &SuiteReport, verbose: bool) -> String {
    let mut s = String::new();
    s.push_str("=== astra-test suite report ===\n");

    let total_prompt: u64 = report.runs.iter().map(|r| r.outcome.prompt_tokens).sum();
    let total_completion: u64 = report
        .runs
        .iter()
        .map(|r| r.outcome.completion_tokens)
        .sum();
    let sum_dur: u64 = report.runs.iter().map(|r| r.outcome.duration_ms).sum();
    let wall_ms = if report.wall_time_ms > 0 {
        report.wall_time_ms
    } else {
        sum_dur
    };
    let wall_secs = wall_ms / 1000;

    s.push_str(&format!(
        "total={} passed={} failed={} | tokens: {}in/{}out | wall: {}m{}s (sum: {}m{}s)\n\n",
        report.total(),
        report.passed(),
        report.failed(),
        total_prompt,
        total_completion,
        wall_secs / 60,
        wall_secs % 60,
        sum_dur / 1000 / 60,
        sum_dur / 1000 % 60,
    ));
    for run in &report.runs {
        let marker = if run.passed { "PASS" } else { "FAIL" };
        let run_suffix = if run.run_index > 0 {
            format!(" run={}", run.run_index)
        } else {
            String::new()
        };
        s.push_str(&format!(
            "[{marker}] case={} model={}{run_suffix} exit={} tools={} dur={}ms turns={}\n",
            run.case_name,
            run.model,
            run.outcome.exit_code,
            run.outcome.tool_calls_count,
            run.outcome.duration_ms,
            run.outcome.turn_rounds,
        ));
        if let Some(ref class) = run.failure_class {
            s.push_str(&format!(
                "    class: {} → {}\n",
                class,
                crate::classify::suggested_action(class)
            ));
        }
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
                s.push_str(&format!("    text: {}\n", truncate(&run.outcome.text, 500)));
            }
            if !run.outcome.stderr.is_empty() {
                s.push_str(&format!(
                    "    stderr: {}\n",
                    truncate(&run.outcome.stderr, 500)
                ));
            }
        }
        // Step-level results for multi-turn cases.
        for step in &run.steps {
            s.push_str(&format!(
                "    step[{}]: dur={}ms tokens={}in/{}out tools={}\n",
                step.step_index,
                step.duration_ms,
                step.outcome.prompt_tokens,
                step.outcome.completion_tokens,
                step.outcome.tool_calls_count,
            ));
            if verbose || !run.passed {
                let text_preview = truncate(&step.outcome.text, 200);
                if !text_preview.is_empty() {
                    s.push_str(&format!("      text: {text_preview}\n"));
                }
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
                    s.push_str(&format!("    journal: ~/.astra/sessions/{id}.jsonl\n"));
                    // Filter to llm_round events first, then project
                    // out the nested tool_calls[].name. Without the
                    // `select(.type==\"llm_round\")` the hint would
                    // mix tool names with `null` from every
                    // llm_request_full / llm_response_full line that
                    // has no tool_calls.
                    s.push_str(&format!(
                        "    hint:    jq -r 'select(.type==\"llm_round\") | .tool_calls[]?.name' ~/.astra/sessions/{id}.jsonl\n"
                    ));
                    s.push_str(&format!(
                        "    hint-steps: jq -r 'select(.event_type==\"ToolCallCompleted\") | .payload.tool_name' ~/.astra/sessions/{id}/step_events.jsonl 2>/dev/null\n"
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

    // Pass rate summary when --runs > 1 (multiple runs per case×model).
    let has_repeats = {
        let mut seen = std::collections::HashSet::new();
        report
            .runs
            .iter()
            .any(|r| !seen.insert((&r.case_name, &r.model)))
    };
    if has_repeats {
        s.push_str("=== pass rate (flaky detection) ===\n");
        let mut groups: std::collections::BTreeMap<(&str, &str), (u32, u32)> =
            std::collections::BTreeMap::new();
        for r in &report.runs {
            let entry = groups.entry((&r.case_name, &r.model)).or_default();
            entry.1 += 1;
            if r.passed {
                entry.0 += 1;
            }
        }
        for ((case, model), (passed, total)) in &groups {
            let pct = (*passed as f64 / *total as f64) * 100.0;
            let marker = if *passed == *total {
                "✓"
            } else if *passed == 0 {
                "✗"
            } else {
                "~"
            };
            s.push_str(&format!(
                "  [{marker}] {case} × {model}: {passed}/{total} ({pct:.0}%)\n"
            ));
        }
        s.push('\n');
    }

    // Capability × model aggregation
    let has_capabilities = report.runs.iter().any(|r| r.capability.is_some());
    if has_capabilities {
        use std::collections::BTreeMap;
        s.push_str("=== capability × model ===\n");
        // (capability_str, model) → (weighted_pass, total_weight)
        let mut cap_groups: BTreeMap<(String, &str), (f64, f64)> = BTreeMap::new();
        for r in &report.runs {
            if let Some(ref cap) = r.capability {
                let entry = cap_groups.entry((cap.to_string(), &r.model)).or_default();
                entry.1 += r.weight;
                if r.passed {
                    entry.0 += r.weight;
                }
            }
        }
        for ((cap, model), (wp, tw)) in &cap_groups {
            let pct = if *tw > 0.0 { wp / tw * 100.0 } else { 0.0 };
            s.push_str(&format!("  {cap} × {model}: {pct:.0}%\n"));
        }
        s.push('\n');
    }

    // Capability × difficulty × model aggregation
    let has_difficulty = report.runs.iter().any(|r| r.difficulty.is_some());
    if has_capabilities && has_difficulty {
        use std::collections::BTreeMap;
        s.push_str("=== capability × difficulty × model ===\n");
        let mut cap_diff_groups: BTreeMap<(String, u8, &str), (f64, f64)> = BTreeMap::new();
        for r in &report.runs {
            if let (Some(cap), Some(diff)) = (&r.capability, r.difficulty) {
                let entry = cap_diff_groups
                    .entry((cap.to_string(), diff, &r.model))
                    .or_default();
                entry.1 += r.weight;
                if r.passed {
                    entry.0 += r.weight;
                }
            }
        }
        for ((cap, diff, model), (wp, tw)) in &cap_diff_groups {
            let pct = if *tw > 0.0 { wp / tw * 100.0 } else { 0.0 };
            s.push_str(&format!("  {cap} × d{diff} × {model}: {pct:.0}%\n"));
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
        out.push_str(&format!("      full:  astra journal digest {id}\n"));
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
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
        }
    }

    fn mk_report_passed() -> SuiteReport {
        SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "c1".into(),
                model: "m".into(),
                passed: true,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
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
                steps: vec![],
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
                failure_class: None,
            }],
            ..Default::default()
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
        r.runs[0].reproducer = Some("/path/to/astra chat -m 'say ok' --model m --json -y".into());
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
                !out.contains(&format!(
                    "jq -r '.tool_calls[]?.name' ~/.astra/sessions/{injection}"
                )),
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
            "",
            "sess;rm",
            "sess|evil",
            "sess\"quote",
            "sess'quote",
            "sess`cmd`",
            "sess$(cmd)",
            "sess>file",
            "sess<file",
            "sess\nline",
            "sess\\/",
            "sess space",
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
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: mk_outcome(),
            criteria: vec![],
            steps: vec![],
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
        });
        assert_eq!(r.total(), 2);
        assert_eq!(r.passed(), 1);
        assert_eq!(r.failed(), 1);
    }

    // ── JSON render fallback (R5 nit a) ──
    //
    // `render_json` returns a structured error blob on serialize
    // failure instead of an empty string. The failure path itself is
    // unreachable today (every `SuiteReport` field is serde-safe),
    // so the tests exercise `format_render_error` directly — it's
    // the pure body of the fallback.

    #[test]
    fn render_error_body_is_valid_json_and_names_reason() {
        let out = format_render_error("something specific broke");
        // Must parse — the whole point of returning a structured blob
        // instead of an empty string is that CI consumers can keep
        // using their JSON parser.
        let parsed: serde_json::Value = serde_json::from_str(&out)
            .expect("error body must be valid JSON so downstream parsers can see it");
        let err = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error field must be a string");
        assert!(
            err.contains("something specific broke"),
            "reason must flow through into the `error` field: {err}"
        );
        assert!(
            err.starts_with("SuiteReport JSON render failed:"),
            "prefix identifies the call site for greppers: {err}"
        );
    }

    #[test]
    fn render_error_escapes_quotes_newlines_and_backslashes() {
        // Regression guard: a reason containing JSON-hostile chars
        // (a reviewer pasting a stack trace with tabs + quotes +
        // backslashes on Windows paths) must still produce valid JSON.
        let nasty = "bad: \"quoted\" \\path\\ with\nnewline and\ttab";
        let out = format_render_error(nasty);
        let parsed: serde_json::Value = serde_json::from_str(&out)
            .expect("escaping must keep the body valid JSON even for nasty input");
        let err = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error string");
        // Payload round-trips byte-for-byte through the JSON unescape.
        assert!(err.contains("\"quoted\""), "quote survived: {err}");
        assert!(err.contains("\\path\\"), "backslashes survived: {err}");
        assert!(err.contains('\n'), "newline survived: {err}");
        assert!(err.contains('\t'), "tab survived: {err}");
    }

    #[test]
    fn render_empty_report_is_valid_json_passed_zero_failed_zero() {
        // Smoke: a zero-case report still round-trips through the
        // public `render(..., Format::Json, …)` surface (happy path,
        // not the fallback) — sanity check that the extraction into
        // `render_json` didn't break the primary path.
        let r = SuiteReport::default();
        let out = render(&r, Format::Json, false);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            parsed
                .get("runs")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn render_text_shows_token_summary() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "a".into(),
                model: "m".into(),
                passed: true,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: {
                    let mut o = RunOutcome::new("m");
                    o.duration_ms = 5000;
                    o
                },
                criteria: vec![],
                steps: vec![],
                failure_class: None,
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("tokens: 0in/0out"),
            "missing token summary: {out}"
        );
        assert!(out.contains("wall: 0m5s"), "missing wall time: {out}");
    }

    #[test]
    fn render_text_shows_pass_rate_when_repeated() {
        let make_run = |passed: bool| CaseRunReport {
            case_name: "flaky".into(),
            model: "m".into(),
            passed,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        };
        let r = SuiteReport {
            runs: vec![make_run(true), make_run(true), make_run(false)],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("pass rate"),
            "missing pass rate section: {out}"
        );
        assert!(out.contains("2/3"), "missing 2/3 count: {out}");
        assert!(out.contains("67%"), "missing percentage: {out}");
    }

    #[test]
    fn render_text_shows_capability_aggregation() {
        let make = |cap, passed| CaseRunReport {
            case_name: "c".into(),
            model: "m".into(),
            passed,
            run_index: 0,
            capability: Some(cap),
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        };
        let r = SuiteReport {
            runs: vec![
                make(crate::case::Capability::ToolUse, true),
                make(crate::case::Capability::ToolUse, false),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("capability × model"),
            "missing cap section: {out}"
        );
        assert!(out.contains("tool_use × m: 50%"), "wrong pct: {out}");
    }

    #[test]
    fn render_text_shows_difficulty_aggregation() {
        let make = |diff, passed| CaseRunReport {
            case_name: "c".into(),
            model: "m".into(),
            passed,
            run_index: 0,
            capability: Some(crate::case::Capability::Reasoning),
            weight: 1.0,
            difficulty: Some(diff),
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        };
        let r = SuiteReport {
            runs: vec![make(1, true), make(3, false)],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("capability × difficulty × model"),
            "missing cap×diff section: {out}"
        );
        assert!(
            out.contains("reasoning × d1 × m: 100%"),
            "d1 should be 100%: {out}"
        );
        assert!(
            out.contains("reasoning × d3 × m: 0%"),
            "d3 should be 0%: {out}"
        );
    }
}
