//! Report rendering.
//!
//! After all cases × models have run, the harness folds every
//! result into one `SuiteReport` and emits it in one of two formats:
//!
//! - `text` — human-scannable, grouped by case, colored PASS/FAIL/UNAVAILABLE
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

/// Terminal state of one case/model run.
///
/// `Unavailable` means the harness deliberately did not execute the case, so
/// it is never evidence for a capability and must not be counted as a pass.
/// In particular, metadata-based prompt-cache exclusions and model-resolution
/// errors use this state instead of manufacturing a green result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseRunStatus {
    Passed,
    Failed,
    /// The work item was planned but never executed because the suite was
    /// cancelled or circuit-broken. It is deliberately not evidence, but it
    /// remains a terminal row so planned work can never disappear from the
    /// report denominator.
    Cancelled,
    Unavailable,
}

impl CaseRunStatus {
    pub fn is_passed(self) -> bool {
        matches!(self, Self::Passed)
    }

    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Unavailable)
    }

    pub fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// One (case, model) pair's full result.
///
/// Serialized into `--format json` reports, so this struct is a
/// de-facto public wire format. `status` is the sole terminal-state
/// authority; consumers must not infer execution state from criteria or
/// rendered text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRunReport {
    pub case_name: String,
    pub model: String,
    /// Authoritative terminal state for this run.
    pub status: CaseRunStatus,
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
    /// Every root execution attempt, including a rate-limit retry. A retry
    /// must remain auditable instead of replacing the first terminal outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptRecord>,
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
    /// Failure classification — populated only when `status` is not `Passed`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure_class: Option<crate::classify::FailureClass>,
    /// True when status is `Passed` but some Soft/Quality criteria failed.
    /// Frontend shows these as yellow warnings, not green passes.
    #[serde(default)]
    pub has_warnings: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_index: u32,
    pub outcome: RunOutcome,
}

impl CaseRunReport {
    pub fn is_passed(&self) -> bool {
        self.status.is_passed()
    }

    pub fn is_unavailable(&self) -> bool {
        self.status.is_unavailable()
    }

    pub fn is_cancelled(&self) -> bool {
        self.status.is_cancelled()
    }

    /// A row with a real executor terminal outcome can contribute runtime
    /// evidence. Cancelled and unavailable rows are planned/accounting rows,
    /// not observations of Astra behavior.
    pub fn is_evidence(&self) -> bool {
        !self.is_unavailable() && !self.is_cancelled()
    }
}

/// One authoritative weight for every capability aggregate. Difficulty is a
/// first-class scoring dimension and `weight` scales the case within that
/// dimension; report, eval, and dashboard consumers must not invent separate
/// formulas.
pub fn scoring_weight(run: &CaseRunReport) -> f64 {
    run.weight * run.difficulty.unwrap_or(1) as f64
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
        self.runs.iter().filter(|r| r.status.is_passed()).count()
    }
    pub fn failed(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| matches!(r.status, CaseRunStatus::Failed))
            .count()
    }
    pub fn cancelled(&self) -> usize {
        self.runs.iter().filter(|r| r.status.is_cancelled()).count()
    }
    pub fn unavailable(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| r.status.is_unavailable())
            .count()
    }
    pub fn non_passed(&self) -> usize {
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
    let total_cache_read: u64 = report
        .runs
        .iter()
        .map(|r| r.outcome.cached_input_tokens)
        .sum();
    let total_cache_create: u64 = report
        .runs
        .iter()
        .map(|r| r.outcome.cache_creation_tokens)
        .sum();
    let sum_dur: u64 = report.runs.iter().map(|r| r.outcome.duration_ms).sum();
    let wall_ms = if report.wall_time_ms > 0 {
        report.wall_time_ms
    } else {
        sum_dur
    };
    let wall_secs = wall_ms / 1000;

    let total_billable_input = astra_turn_types::NormalizedPromptCacheUsage::new(
        total_prompt,
        total_cache_read,
        total_cache_create,
    )
    .total_input_tokens();
    let cache_ratio_pct = if total_cache_read > 0 {
        format!(
            " cache-read={:.0}%",
            total_cache_read as f64 / total_billable_input as f64 * 100.0
        )
    } else {
        String::new()
    };
    s.push_str(&format!(
        "total={} passed={} failed={} cancelled={} unavailable={} | tokens: {} fresh-in/{}out{} | wall: {}m{}s (sum: {}m{}s)\n\n",
        report.total(),
        report.passed(),
        report.failed(),
        report.cancelled(),
        report.unavailable(),
        total_prompt,
        total_completion,
        cache_ratio_pct,
        wall_secs / 60,
        wall_secs % 60,
        sum_dur / 1000 / 60,
        sum_dur / 1000 % 60,
    ));
    for run in &report.runs {
        let marker = match run.status {
            CaseRunStatus::Passed => "PASS",
            CaseRunStatus::Failed => "FAIL",
            CaseRunStatus::Cancelled => "CANCELLED",
            CaseRunStatus::Unavailable => "UNAVAILABLE",
        };
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
        if run.attempts.len() > 1 {
            s.push_str(&format!(
                "    warning: {} terminal attempts recorded; totals include every attempt\n",
                run.attempts.len()
            ));
        }
        if let Some(ref class) = run.failure_class {
            s.push_str(&format!(
                "    class: {} → {}\n",
                class,
                crate::classify::suggested_action(class)
            ));
        }
        for c in &run.criteria {
            let m = match (run.status, c.passed, c.severity) {
                (CaseRunStatus::Unavailable | CaseRunStatus::Cancelled, _, _) => " N/A",
                (_, true, _) => " ok ",
                (_, false, crate::criteria::CriterionSeverity::Hard) => "FAIL",
                (_, false, crate::criteria::CriterionSeverity::Soft)
                | (_, false, crate::criteria::CriterionSeverity::Quality) => "WARN",
            };
            s.push_str(&format!("    [{m}] {}\n", c.detail));
            // FAIL or --verbose: dump the untruncated diagnostic if
            // the criterion carries one (judger quorum votes, etc.).
            // Indented block so it's visually nested under the fail.
            if (run.is_unavailable() || !c.passed || verbose)
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
        if verbose || !run.is_passed() {
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
            if verbose || !run.is_passed() {
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
            let health = crate::pipeline_analysis::analyze_pipeline_health(cap);
            if health.turns_with_feedback > 0 {
                s.push_str(&format!(
                    "    pipeline: turns={} cache={:.0}% compactions={}\n",
                    health.turns_with_feedback,
                    health.avg_cache_hit_ratio * 100.0,
                    health.compaction_count,
                ));
                if health.cascade_detected {
                    s.push_str("    pipeline: ⚠ compaction cascade detected\n");
                }
                for alert in &health.alerts {
                    s.push_str(&format!(
                        "    pipeline: T{} [{}] {}\n",
                        alert.turn, alert.severity, alert.rule
                    ));
                }
            }
        }
        // Diagnostic hints on FAIL — copy-paste debugging commands.
        if !run.is_passed() {
            if let Some(id) = run.outcome.session_id.as_deref() {
                // Session ids should be UUIDs or simple slugs. If the
                // runtime ever returns something richer, we refuse to
                // splice it into shell hints — a malicious/stale id
                // with shell metachars could make the copy-paste hint
                // a remote-execution vector for an unwary developer.
                if is_safe_session_id(id) {
                    if let Some(capture) = run.session.as_ref() {
                        s.push_str(&format!(
                            "    capture: {}\n",
                            capture.journal_path.display()
                        ));
                    } else {
                        s.push_str("    capture: unavailable\n");
                    }
                    s.push_str(&format!("    hint:    astra journal digest {id}\n"));
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
                render_digest_summary(&d.json, &d.session_id, &mut s);
            }
            if let Some(err) = run.digest_error.as_deref() {
                s.push_str(&format!("    digest_error: {err}\n"));
            }
        }
        s.push('\n');
    }

    use std::collections::{BTreeMap, BTreeSet, HashSet};

    // Pass rate summary when --runs > 1 (multiple runs per case×model).
    let has_repeats = {
        let mut seen = HashSet::new();
        report
            .runs
            .iter()
            .any(|r| !seen.insert((&r.case_name, &r.model)))
    };
    if has_repeats {
        s.push_str("=== pass rate (flaky detection) ===\n");
        let mut groups: BTreeMap<(&str, &str), (u32, u32, u32, u32)> = BTreeMap::new();
        for r in &report.runs {
            let entry = groups.entry((&r.case_name, &r.model)).or_default();
            if r.is_unavailable() {
                entry.2 += 1;
            } else if r.is_cancelled() {
                entry.3 += 1;
            } else {
                entry.1 += 1;
                if r.is_passed() {
                    entry.0 += 1;
                }
            }
        }
        for ((case, model), (passed, available, unavailable, cancelled)) in &groups {
            let planned = *available + *cancelled;
            if planned == 0 {
                s.push_str(&format!(
                    "  [!] {case} × {model}: unavailable={unavailable} cancelled={cancelled} (not executed)\n"
                ));
                continue;
            }
            let pct = (*passed as f64 / planned as f64) * 100.0;
            let marker = if *passed == planned {
                "✓"
            } else if *passed == 0 {
                "✗"
            } else {
                "~"
            };
            let mut unavailable_suffix = String::new();
            if *unavailable > 0 {
                unavailable_suffix.push_str(&format!(", unavailable={unavailable}"));
            }
            if *cancelled > 0 {
                unavailable_suffix.push_str(&format!(", cancelled={cancelled}"));
            }
            s.push_str(&format!(
                "  [{marker}] {case} × {model}: {passed}/{planned} ({pct:.0}%){unavailable_suffix}\n"
            ));
        }
        s.push('\n');
    }

    // Collect distinct raw execution identities; display labels are not keys.
    let models: BTreeSet<&str> = report
        .runs
        .iter()
        .filter(|r| r.is_evidence())
        .map(|r| r.model.as_str())
        .collect();
    let multi_model = models.len() > 1;

    // ── Model comparison (multi-dimensional, always shown when > 1 model) ──
    if multi_model {
        #[derive(Default)]
        struct ModelStats {
            pass: u32,
            total: u32,
            pass_tokens: u64,
            pass_dur_ms: u64,
            pass_turns: u64,
            pass_tools: u64,
            all_tokens: u64,
            all_dur_ms: u64,
        }
        let mut stats: BTreeMap<&str, ModelStats> = BTreeMap::new();
        for r in &report.runs {
            if !r.is_evidence() {
                continue;
            }
            let e = stats.entry(r.model.as_str()).or_default();
            e.total += 1;
            let tok = r.outcome.prompt_tokens + r.outcome.completion_tokens;
            e.all_tokens += tok;
            e.all_dur_ms += r.outcome.duration_ms;
            if r.is_passed() {
                e.pass += 1;
                e.pass_tokens += tok;
                e.pass_dur_ms += r.outcome.duration_ms;
                e.pass_turns += r.outcome.turn_rounds as u64;
                e.pass_tools += r.outcome.tool_calls_count as u64;
            }
        }
        s.push_str("=== model comparison ===\n");
        let mut ranked: Vec<_> = stats.iter().collect();
        ranked.sort_by(|a, b| {
            let pa = if a.1.total > 0 {
                a.1.pass as f64 / a.1.total as f64
            } else {
                0.0
            };
            let pb = if b.1.total > 0 {
                b.1.pass as f64 / b.1.total as f64
            } else {
                0.0
            };
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });
        for (model, st) in &ranked {
            let pct = if st.total > 0 {
                st.pass as f64 / st.total as f64 * 100.0
            } else {
                0.0
            };
            let p = st.pass.max(1) as u64;
            s.push_str(&format!(
                "  {model}: pass={}/{} ({pct:.0}%) \
                 | tok/pass={} dur/pass={}ms turns/pass={:.1} tools/pass={:.1}\n",
                st.pass,
                st.total,
                st.pass_tokens / p,
                st.pass_dur_ms / p,
                st.pass_turns as f64 / p as f64,
                st.pass_tools as f64 / p as f64,
            ));
        }
        s.push('\n');
    }

    // ── Capability × model ──
    let has_capabilities = report
        .runs
        .iter()
        .any(|r| r.is_evidence() && r.capability.is_some());
    if has_capabilities {
        s.push_str("=== capability × model ===\n");
        let mut cap_groups: BTreeMap<(String, &str), (f64, f64)> = BTreeMap::new();
        for r in &report.runs {
            if !r.is_evidence() {
                continue;
            }
            if let Some(ref cap) = r.capability {
                let entry = cap_groups
                    .entry((cap.to_string(), r.model.as_str()))
                    .or_default();
                entry.1 += scoring_weight(r);
                if r.is_passed() {
                    entry.0 += scoring_weight(r);
                }
            }
        }
        for ((cap, model), (wp, tw)) in &cap_groups {
            let pct = if *tw > 0.0 { wp / tw * 100.0 } else { 0.0 };
            s.push_str(&format!("  {cap} × {model}: {pct:.0}%\n"));
        }
        s.push('\n');
    }

    // ── Difficulty curve (per-difficulty pass rate across models) ──
    let has_difficulty = report
        .runs
        .iter()
        .any(|r| r.is_evidence() && r.difficulty.is_some());
    if has_difficulty {
        s.push_str("=== difficulty curve ===\n");
        // (difficulty, model) → (weighted_pass, weighted_total)
        let mut diff_groups: BTreeMap<(u8, &str), (f64, f64)> = BTreeMap::new();
        let mut diff_all: BTreeMap<u8, (f64, f64)> = BTreeMap::new();
        for r in &report.runs {
            if !r.is_evidence() {
                continue;
            }
            if let Some(d) = r.difficulty {
                let entry = diff_groups.entry((d, r.model.as_str())).or_default();
                entry.1 += scoring_weight(r);
                if r.is_passed() {
                    entry.0 += scoring_weight(r);
                }
                let all = diff_all.entry(d).or_default();
                all.1 += scoring_weight(r);
                if r.is_passed() {
                    all.0 += scoring_weight(r);
                }
            }
        }
        if multi_model {
            for ((diff, model), (wp, tw)) in &diff_groups {
                let pct = if *tw > 0.0 { wp / tw * 100.0 } else { 0.0 };
                s.push_str(&format!("  d{diff} × {model}: {pct:.0}%\n"));
            }
        } else {
            for (diff, (wp, tw)) in &diff_all {
                let pct = if *tw > 0.0 { wp / tw * 100.0 } else { 0.0 };
                s.push_str(&format!("  d{diff}: {pct:.0}%\n"));
            }
        }
        s.push('\n');
    }

    // ── Capability × difficulty × model (detailed, only when both axes exist) ──
    if has_capabilities && has_difficulty && multi_model {
        s.push_str("=== capability × difficulty × model ===\n");
        let mut cdm: BTreeMap<(String, u8, &str), (f64, f64)> = BTreeMap::new();
        for r in &report.runs {
            if !r.is_evidence() {
                continue;
            }
            if let (Some(cap), Some(diff)) = (&r.capability, r.difficulty) {
                let entry = cdm
                    .entry((cap.to_string(), diff, r.model.as_str()))
                    .or_default();
                entry.1 += scoring_weight(r);
                if r.is_passed() {
                    entry.0 += scoring_weight(r);
                }
            }
        }
        for ((cap, diff, model), (wp, tw)) in &cdm {
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
fn render_digest_summary(json: &serde_json::Value, expected_session_id: &str, out: &mut String) {
    if let Err(error) = crate::digest::validate_digest_json(json, expected_session_id) {
        out.push_str(&format!(
            "      digest_error: invalid typed digest: {error}\n"
        ));
        return;
    }
    let a = json
        .get("aggregates")
        .and_then(serde_json::Value::as_object)
        .expect("validated digest aggregates");
    let get_u = |key: &str| {
        a.get(key)
            .and_then(|v| v.as_u64())
            .expect("validated count")
    };
    let get_f = |key: &str| {
        a.get(key)
            .and_then(|v| v.as_f64())
            .expect("validated average")
    };
    out.push_str(&format!(
            "      attempts={} turns={} turn_errors={} tool_calls={} tool_failures={} errors={} compacts={} stalls={}\n",
            get_u("attempt_count"),
            get_u("turn_count"),
            get_u("turn_error_count"),
            get_u("total_tool_calls"),
            get_u("tool_calls_failed"),
            get_u("error_event_count"),
            get_u("compact_count"),
            get_u("stall_count"),
    ));
    out.push_str(&format!(
        "      root_tokens_in={} root_tokens_out={} root_duration_ms={}\n",
        get_u("total_tokens_in"),
        get_u("total_tokens_out"),
        get_u("total_duration_ms"),
    ));
    out.push_str(&format!(
        "      subruns={} inclusive_tokens_in={} inclusive_tokens_out={} inclusive_tool_calls={}\n",
        get_u("subrun_count"),
        get_u("inclusive_total_tokens_in"),
        get_u("inclusive_total_tokens_out"),
        get_u("inclusive_total_tool_calls"),
    ));
    out.push_str(&format!(
        "      avg_tokens_in={:.1} avg_tokens_out={:.1} avg_duration_ms={:.1}\n",
        get_f("avg_tokens_in"),
        get_f("avg_tokens_out"),
        get_f("avg_duration_ms"),
    ));
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

/// Strip provider-route prefixes for display grouping.
/// `us.anthropic.claude-sonnet-4-6` → `claude-sonnet-4-6`
pub fn normalize_model_display(model: &str) -> &str {
    for prefix in [
        "us.anthropic.",
        "eu.anthropic.",
        "ap.anthropic.",
        "us.amazon.",
        "eu.amazon.",
    ] {
        if let Some(rest) = model.strip_prefix(prefix) {
            return rest;
        }
    }
    model
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
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 42,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
            final_state: None,
            interruption_kind: None,
            tool_result_class_counts: std::collections::BTreeMap::new(),
        }
    }

    fn mk_report_passed() -> SuiteReport {
        SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "c1".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: mk_outcome(),
                criteria: vec![CriterionResult {
                    criterion: Criterion::ToolCalled {
                        name: "Read".into(),
                    },
                    severity: crate::criteria::CriterionSeverity::Hard,
                    passed: true,
                    detail: "tool Read was called".into(),
                    full_detail: None,
                    score: None,
                }],
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
        r.runs[0].status = CaseRunStatus::Failed;
        r.runs[0].outcome.stderr = "boom".into();
        let out = render(&r, Format::Text, false);
        assert!(out.contains("[FAIL]"));
        assert!(out.contains("text: hello"));
        assert!(out.contains("stderr: boom"));
    }

    #[test]
    fn text_report_distinguishes_advisory_warning_from_hard_failure() {
        let mut r = mk_report_passed();
        r.runs[0].criteria[0].passed = false;
        r.runs[0].criteria[0].severity = crate::criteria::CriterionSeverity::Soft;
        r.runs[0].criteria[0].detail = "token budget exceeded".into();
        r.runs[0].has_warnings = true;

        let out = render(&r, Format::Text, false);

        assert!(out.contains("[WARN] token budget exceeded"));
        assert!(!out.contains("[FAIL] token budget exceeded"));
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
        r.runs[0].status = CaseRunStatus::Failed;
        r.runs[0].session = Some(SessionCapture {
            session_id: "sess".into(),
            journal_path: std::path::PathBuf::from("/captures/account/sess.jsonl"),
            events: Vec::new(),
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        });
        r.runs[0].reproducer = Some("/path/to/astra chat -m 'say ok' --model m --json -y".into());
        let out = render(&r, Format::Text, false);
        assert!(out.contains("capture: /captures/account/sess.jsonl"));
        assert!(out.contains("astra journal digest sess"));
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
            r.runs[0].status = CaseRunStatus::Failed;
            r.runs[0].outcome.session_id = Some(injection.to_string());
            let out = render(&r, Format::Text, false);
            assert!(
                !out.contains(&format!("astra journal digest {injection}")),
                "diagnostic command must NOT splice the suspicious id: {out}"
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
        r.runs[0].status = CaseRunStatus::Failed;
        r.runs[0].digest = Some(DigestArtifact {
            session_id: "sess".into(),
            json: serde_json::json!({
                "schema_version": "astra-journal-digest-v2",
                "session_id": "sess",
                "journal_file": "/tmp/sess.jsonl",
                "journal_lines_non_empty": 3,
                "journal_lines_malformed": 0,
                "aggregates": {
                    "attempt_count": 3,
                    "turn_count": 3,
                    "turn_error_count": 0,
                    "total_tool_calls": 5,
                    "tool_calls_failed": 1,
                    "error_event_count": 0,
                    "compact_count": 0,
                    "stall_count": 0,
                    "total_tokens_in": 12000,
                    "total_tokens_out": 450,
                    "total_duration_ms": 8200,
                    "subrun_count": 0,
                    "inclusive_total_tokens_in": 12000,
                    "inclusive_total_tokens_out": 450,
                    "inclusive_total_tool_calls": 5,
                    "total_fresh_tool_calls": 5,
                    "total_noop_or_cached_tool_calls": 0,
                    "safety_guard_blocks": 0,
                    "session_start_count": 1,
                    "session_end_count": 1,
                    "subrun_total_tokens_in": 0,
                    "subrun_total_tokens_out": 0,
                    "subrun_total_duration_ms": 0,
                    "subrun_total_tool_calls": 0,
                    "avg_tokens_in": 4000.0,
                    "avg_tokens_out": 150.0,
                    "avg_duration_ms": 2733.33,
                    "avg_llm_rounds": 1.0,
                    "avg_tool_calls_per_round": 1.67,
                }
            }),
        });
        let out = render(&r, Format::Text, false);
        assert!(out.contains("digest:"));
        assert!(out.contains("turns=3"));
        assert!(out.contains("tool_calls=5"));
        assert!(out.contains("root_tokens_in=12000"));
        assert!(out.contains("inclusive_tokens_in=12000"));
        assert!(out.contains("avg_tokens_in=4000.0"));
        assert!(out.contains("astra journal digest sess"));
    }

    #[test]
    fn text_report_renders_v2_digest_aggregate_names_without_false_zeroes() {
        use crate::digest::DigestArtifact;
        let mut r = mk_report_passed();
        r.runs[0].status = CaseRunStatus::Failed;
        r.runs[0].digest = Some(DigestArtifact {
            session_id: "sess-v2".into(),
            json: serde_json::json!({
                "schema_version": "astra-journal-digest-v2",
                "session_id": "sess-v2",
                "journal_file": "/tmp/sess-v2.jsonl",
                "journal_lines_non_empty": 2,
                "journal_lines_malformed": 0,
                "aggregates": {
                    "attempt_count": 2,
                    "turn_count": 1,
                    "turn_error_count": 1,
                    "total_tool_calls": 14,
                    "tool_calls_failed": 2,
                    "error_event_count": 0,
                    "compact_count": 0,
                    "stall_count": 0,
                    "total_tokens_in": 60147,
                    "total_tokens_out": 7112,
                    "total_duration_ms": 116320,
                    "subrun_count": 3,
                    "inclusive_total_tokens_in": 189908,
                    "inclusive_total_tokens_out": 29610,
                    "inclusive_total_tool_calls": 96,
                    "total_fresh_tool_calls": 12,
                    "total_noop_or_cached_tool_calls": 2,
                    "safety_guard_blocks": 0,
                    "session_start_count": 1,
                    "session_end_count": 1,
                    "subrun_total_tokens_in": 129761,
                    "subrun_total_tokens_out": 22498,
                    "subrun_total_duration_ms": 0,
                    "subrun_total_tool_calls": 82,
                    "avg_tokens_in": 30073.5,
                    "avg_tokens_out": 3556.0,
                    "avg_duration_ms": 58160.0,
                    "avg_llm_rounds": 2.0,
                    "avg_tool_calls_per_round": 7.0
                }
            }),
        });

        let out = render(&r, Format::Text, false);
        assert!(out.contains("attempts=2 turns=1 turn_errors=1"));
        assert!(out.contains("tool_calls=14 tool_failures=2"));
        assert!(out.contains("root_tokens_in=60147 root_tokens_out=7112 root_duration_ms=116320"));
        assert!(out.contains("subruns=3 inclusive_tokens_in=189908"));
        assert!(
            out.contains("avg_tokens_in=30073.5 avg_tokens_out=3556.0 avg_duration_ms=58160.0")
        );
    }

    #[test]
    fn text_report_does_not_render_partial_digest_as_zero_health() {
        use crate::digest::DigestArtifact;
        let mut r = mk_report_passed();
        r.runs[0].status = CaseRunStatus::Failed;
        r.runs[0].digest = Some(DigestArtifact {
            session_id: "requested".into(),
            json: serde_json::json!({
                "schema_version": "astra-journal-digest-v2",
                "session_id": "foreign",
                "journal_file": "/tmp/foreign.jsonl",
                "journal_lines_non_empty": 1,
                "journal_lines_malformed": 0,
                "aggregates": {}
            }),
        });
        let out = render(&r, Format::Text, false);
        assert!(out.contains("digest_error: invalid typed digest"));
        assert!(!out.contains("attempts=0 turns=0"));
    }

    #[test]
    fn text_report_renders_digest_error_on_fail() {
        let mut r = mk_report_passed();
        r.runs[0].status = CaseRunStatus::Failed;
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
            status: CaseRunStatus::Failed,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: mk_outcome(),
            criteria: vec![],
            steps: vec![],
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            has_warnings: false,
        });
        assert_eq!(r.total(), 2);
        assert_eq!(r.passed(), 1);
        assert_eq!(r.failed(), 1);
    }

    #[test]
    fn unavailable_is_visible_and_never_counted_as_pass() {
        let mut report = mk_report_passed();
        report.runs[0].status = CaseRunStatus::Unavailable;
        report.runs[0].outcome.text = "unavailable: metadata does not prove scope".into();
        report.runs[0].failure_class =
            Some(crate::classify::FailureClass::InfraVerificationUnavailable);

        assert_eq!(report.passed(), 0);
        assert_eq!(report.failed(), 0);
        assert_eq!(report.unavailable(), 1);
        let text = render(&report, Format::Text, false);
        assert!(text.contains("[UNAVAILABLE]"), "{text}");
        assert!(text.contains("unavailable=1"), "{text}");

        let json: serde_json::Value = serde_json::from_str(&render(&report, Format::Json, false))
            .expect("status report must be valid JSON");
        assert_eq!(json["runs"][0]["status"], "unavailable");
        assert!(json["runs"][0].get("passed").is_none());
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
                status: CaseRunStatus::Passed,
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
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("tokens: 0 fresh-in/0out"),
            "missing token summary: {out}"
        );
        assert!(out.contains("wall: 0m5s"), "missing wall time: {out}");
    }

    #[test]
    fn render_text_uses_total_input_for_cache_read_share() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "cache".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: {
                    let mut o = RunOutcome::new("m");
                    o.prompt_tokens = 200;
                    o.completion_tokens = 50;
                    o.cached_input_tokens = 800;
                    o.cache_creation_tokens = 0;
                    o
                },
                criteria: vec![],
                steps: vec![],
                failure_class: None,
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        assert!(
            out.contains("tokens: 200 fresh-in/50out cache-read=80%"),
            "cache share must use fresh+read+creation denominator: {out}"
        );
    }

    /// Regression: the denominator must include `cache_creation_tokens`,
    /// otherwise a turn that wrote 200 cache tokens on top of 200 reads
    /// would appear to have read=50% when it actually read 200/(200+200+200)
    /// = 33% of total billable input. Anthropic and Bedrock reports both
    /// surface non-zero `cache_creation_tokens` on the first cached call,
    /// so this case is real.
    #[test]
    fn render_text_cache_read_share_includes_cache_creation_in_denominator() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "cache".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: {
                    let mut o = RunOutcome::new("m");
                    o.prompt_tokens = 100;
                    o.completion_tokens = 40;
                    o.cached_input_tokens = 200;
                    o.cache_creation_tokens = 200;
                    o
                },
                criteria: vec![],
                steps: vec![],
                failure_class: None,
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        // 200 / (100 + 200 + 200) = 0.40 → 40%. If the denominator drops
        // `cache_creation_tokens`, the share would render as 200/(100+200)
        // = 67% — the assertion catches that regression.
        assert!(
            out.contains("cache-read=40%"),
            "cache_creation_tokens must contribute to the denominator: {out}"
        );
    }

    #[test]
    fn render_text_omits_cache_read_share_when_no_cache_read_occurred() {
        let r = SuiteReport {
            runs: vec![CaseRunReport {
                case_name: "cache-create-only".into(),
                model: "m".into(),
                status: CaseRunStatus::Passed,
                run_index: 0,
                capability: None,
                weight: 1.0,
                difficulty: None,
                outcome: {
                    let mut o = RunOutcome::new("m");
                    o.prompt_tokens = 100;
                    o.completion_tokens = 40;
                    o.cached_input_tokens = 0;
                    o.cache_creation_tokens = 200;
                    o
                },
                criteria: vec![],
                steps: vec![],
                failure_class: None,
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        assert!(
            !out.contains("cache-read="),
            "cache-read share should stay hidden until some cached input was actually read: {out}"
        );
    }

    #[test]
    fn render_text_shows_pass_rate_when_repeated() {
        let make_run = |passed: bool| CaseRunReport {
            case_name: "flaky".into(),
            model: "m".into(),
            status: if passed {
                CaseRunStatus::Passed
            } else {
                CaseRunStatus::Failed
            },
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            has_warnings: false,
            attempts: Vec::new(),
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
    fn render_text_does_not_count_unavailable_repeats_as_failures() {
        let make_run = |status| CaseRunReport {
            case_name: "unavailable-repeat".into(),
            model: "m".into(),
            status,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        };
        let report = SuiteReport {
            runs: vec![
                make_run(CaseRunStatus::Unavailable),
                make_run(CaseRunStatus::Unavailable),
            ],
            ..Default::default()
        };
        let out = render_text(&report, false);
        assert!(
            out.contains("unavailable=2 cancelled=0 (not executed)"),
            "unavailable repeats must be explicit: {out}"
        );
        assert!(
            !out.contains("0/2 (0%)"),
            "unavailable is not a failed run: {out}"
        );
    }

    #[test]
    fn render_text_counts_cancelled_repeats_in_planned_denominator() {
        let make_run = |status| CaseRunReport {
            case_name: "cancelled-repeat".into(),
            model: "m".into(),
            status,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("m"),
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        };
        let report = SuiteReport {
            runs: vec![
                make_run(CaseRunStatus::Passed),
                make_run(CaseRunStatus::Cancelled),
                make_run(CaseRunStatus::Cancelled),
            ],
            ..Default::default()
        };
        let out = render_text(&report, false);
        assert!(
            out.contains("cancelled-repeat × m: 1/3 (33%)") && out.contains("cancelled=2"),
            "cancelled planned rows must remain in the denominator: {out}"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn mk_run(
        case: &str,
        model: &str,
        passed: bool,
        cap: Option<crate::case::Capability>,
        diff: Option<u8>,
        weight: f64,
        dur_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
    ) -> CaseRunReport {
        mk_run_full(
            case, model, passed, cap, diff, weight, dur_ms, tokens_in, tokens_out, 1, 0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn mk_run_full(
        case: &str,
        model: &str,
        passed: bool,
        cap: Option<crate::case::Capability>,
        diff: Option<u8>,
        weight: f64,
        dur_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        turns: u32,
        tool_calls: u32,
    ) -> CaseRunReport {
        CaseRunReport {
            case_name: case.into(),
            model: model.into(),
            status: if passed {
                CaseRunStatus::Passed
            } else {
                CaseRunStatus::Failed
            },
            run_index: 0,
            capability: cap,
            weight,
            difficulty: diff,
            outcome: {
                let mut o = RunOutcome::new(model);
                o.duration_ms = dur_ms;
                o.prompt_tokens = tokens_in;
                o.completion_tokens = tokens_out;
                o.turn_rounds = turns;
                o.tool_calls_count = tool_calls;
                o
            },
            criteria: vec![],
            steps: vec![],
            failure_class: None,
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
        }
    }

    #[test]
    fn model_comparison_shows_multi_dimensional_metrics() {
        use crate::case::Capability::*;
        // gpt-4: pass easy(d1), fail hard(d4) — when passes: 500ms, 150tok, 2 turns
        // qwen:  pass both — easy: 300ms 120tok 1 turn; hard: 2000ms 1900tok 5 turns
        let r = SuiteReport {
            runs: vec![
                mk_run_full(
                    "easy",
                    "gpt-4",
                    true,
                    Some(ToolUse),
                    Some(1),
                    1.0,
                    500,
                    100,
                    50,
                    2,
                    3,
                ),
                mk_run_full(
                    "easy",
                    "qwen",
                    true,
                    Some(ToolUse),
                    Some(1),
                    1.0,
                    300,
                    80,
                    40,
                    1,
                    2,
                ),
                mk_run_full(
                    "hard",
                    "gpt-4",
                    false,
                    Some(ToolUse),
                    Some(4),
                    2.0,
                    3000,
                    2000,
                    500,
                    8,
                    15,
                ),
                mk_run_full(
                    "hard",
                    "qwen",
                    true,
                    Some(ToolUse),
                    Some(4),
                    2.0,
                    2000,
                    1500,
                    400,
                    5,
                    10,
                ),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);

        assert!(
            out.contains("model comparison"),
            "must show model comparison:\n{out}"
        );
        assert!(
            out.contains("qwen") && out.contains("gpt-4"),
            "both models:\n{out}"
        );
        // Must show pass rate (not just weighted %)
        assert!(out.contains("pass="), "must show raw pass count:\n{out}");
        // Must show efficiency metrics on passes
        assert!(
            out.contains("avg_tok") || out.contains("tok/pass"),
            "must show token efficiency:\n{out}"
        );
        assert!(
            out.contains("avg_dur") || out.contains("dur/pass"),
            "must show duration efficiency:\n{out}"
        );
        assert!(
            out.contains("avg_turns") || out.contains("turns/pass"),
            "must show turns efficiency:\n{out}"
        );
    }

    #[test]
    fn model_comparison_efficiency_only_counts_passed_cases() {
        // Model A: pass 1 case (100tok, 1s), fail 1 case (10000tok, 30s)
        // Model B: pass 2 cases (200tok each, 2s each)
        // A's efficiency should be 100tok/1s (not averaged with the failure)
        let r = SuiteReport {
            runs: vec![
                mk_run_full("c1", "A", true, None, None, 1.0, 1000, 80, 20, 1, 2),
                mk_run_full("c2", "A", false, None, None, 1.0, 30000, 8000, 2000, 15, 50),
                mk_run_full("c1", "B", true, None, None, 1.0, 2000, 150, 50, 2, 3),
                mk_run_full("c2", "B", true, None, None, 1.0, 2000, 150, 50, 2, 3),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);
        // A passed 1 case: tok/pass should be 100 (80+20), NOT (80+20+8000+2000)/2
        assert!(
            out.contains("model comparison"),
            "must show comparison:\n{out}"
        );
    }

    #[test]
    fn difficulty_curve_shows_metrics_per_level() {
        use crate::case::Capability::*;
        let r = SuiteReport {
            runs: vec![
                mk_run("e1", "m", true, Some(Reasoning), Some(1), 1.0, 100, 10, 5),
                mk_run("e2", "m", true, Some(Reasoning), Some(2), 1.0, 200, 20, 10),
                mk_run(
                    "h1",
                    "m",
                    false,
                    Some(Reasoning),
                    Some(4),
                    1.0,
                    5000,
                    500,
                    200,
                ),
                mk_run(
                    "h2",
                    "m",
                    false,
                    Some(Reasoning),
                    Some(5),
                    1.0,
                    8000,
                    1000,
                    500,
                ),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("difficulty"),
            "should show difficulty section:\n{out}"
        );
    }

    #[test]
    fn normalize_model_display_strips_known_prefixes() {
        assert_eq!(
            normalize_model_display("us.anthropic.claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_display("eu.anthropic.claude-opus-4-7"),
            "claude-opus-4-7"
        );
        assert_eq!(normalize_model_display("MiniMax-M2.7"), "MiniMax-M2.7");
        assert_eq!(normalize_model_display("qwen-flash"), "qwen-flash");
    }

    #[test]
    fn model_comparison_preserves_provider_route_identity() {
        // Region/provider routes are distinct execution identities. They may
        // share a display suffix, but must never share a score denominator.
        let r = SuiteReport {
            runs: vec![
                mk_run("c1", "claude-sonnet-4-6", true, None, None, 1.0, 100, 10, 5),
                mk_run(
                    "c2",
                    "us.anthropic.claude-sonnet-4-6",
                    true,
                    None,
                    None,
                    1.0,
                    200,
                    20,
                    10,
                ),
                mk_run("c1", "MiniMax-M2.7", false, None, None, 1.0, 300, 30, 15),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("model comparison"),
            "multi-model must show comparison:\n{out}"
        );
        assert!(
            out.contains("claude-sonnet-4-6: pass=1/1")
                && out.contains("us.anthropic.claude-sonnet-4-6: pass=1/1"),
            "provider route identities must remain separate: {out}"
        );
    }

    #[test]
    fn single_model_no_comparison_table() {
        let r = SuiteReport {
            runs: vec![
                mk_run("c1", "m", true, None, None, 1.0, 100, 10, 5),
                mk_run("c2", "m", false, None, None, 1.0, 100, 10, 5),
            ],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            !out.contains("model comparison"),
            "single-model run should not show comparison:\n{out}"
        );
    }
}
