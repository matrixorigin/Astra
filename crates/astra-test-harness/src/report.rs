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
    /// Complete journal captures for every root attempt owned by this run.
    /// This archive survives destructive session cleanup, including retries
    /// that received a different server session identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_captures: Vec<SessionCapture>,
    /// Typed execution attribution derived from the captured journal. This
    /// stays optional because cases without durable capture cannot certify
    /// these counters.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub execution: Option<crate::pipeline_analysis::ExecutionTraceReport>,
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
    /// Cleanup is a separate harness outcome. A product failure is never
    /// overwritten by teardown/capture errors, but the case stays failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_errors: Vec<String>,
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

fn auxiliary_judgment_lines(run: &CaseRunReport) -> Vec<String> {
    let execution_count = run.attempts.len() + run.steps.len();
    if execution_count == 0 {
        return auxiliary_lines_for_outcome("", &run.outcome);
    }

    let mut lines = Vec::new();
    for attempt in &run.attempts {
        let label = (execution_count > 1).then(|| format!("attempt[{}]", attempt.attempt_index));
        lines.extend(auxiliary_lines_for_outcome(
            label.as_deref().unwrap_or(""),
            &attempt.outcome,
        ));
    }
    for step in &run.steps {
        let label = (execution_count > 1).then(|| format!("step[{}]", step.step_index));
        lines.extend(auxiliary_lines_for_outcome(
            label.as_deref().unwrap_or(""),
            &step.outcome,
        ));
    }
    lines
}

fn auxiliary_lines_for_outcome(label: &str, outcome: &RunOutcome) -> Vec<String> {
    let heading = if label.is_empty() {
        "    auxiliary".to_owned()
    } else {
        format!("    auxiliary {label}")
    };
    let Some(capture) = outcome.explain_capture.as_ref() else {
        return vec![format!("{heading}: evidence not captured\n")];
    };
    let Some(graph) = capture.canonical_graph() else {
        return vec![format!(
            "{heading}: evidence unavailable (capture incomplete)\n"
        )];
    };

    let details = graph.auxiliary_details();
    let usage = graph.auxiliary_usage_snapshot();
    let calls = details
        .iter()
        .enumerate()
        .flat_map(|(scope, (_, details))| details.calls.iter().map(move |call| (scope, call)))
        .collect::<Vec<_>>();
    let call_timing_truncated = details.iter().any(|(_, details)| details.truncated);
    let terminal_turns = graph
        .nodes()
        .iter()
        .filter(|node| {
            node.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::Turn && node.terminal_observed
        })
        .collect::<Vec<_>>();
    let call_timing_scope_complete = !terminal_turns.is_empty()
        && terminal_turns
            .iter()
            .all(|node| node.auxiliary_details.is_some());
    let scope_coverage = graph.execution_scope_coverage();
    let scope_coverage_complete = !scope_coverage.is_empty()
        && scope_coverage
            .iter()
            .all(|scope| scope.terminal_turn_observed && scope.auxiliary_snapshot_observed)
        && !graph.auxiliary_usage_unavailable();
    let usage_snapshot_complete = scope_coverage_complete
        && !graph.auxiliary_usage_truncated()
        && !graph.auxiliary_capture_conflicted()
        && graph.auxiliary_usage_conflict_count() == 0;
    let mut lines = Vec::new();
    for (scope, (_, detail)) in details.iter().enumerate() {
        let scope_label = if details.len() > 1 {
            format!(" scope[{scope}]")
        } else {
            String::new()
        };
        if let Some(admission) = detail.admission.as_ref() {
            let status = match admission.status {
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Accepted => "accepted",
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Rejected => "rejected",
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::Unavailable => {
                    "unavailable"
                }
                astra_turn_types::ExplainAnalyzeAdmissionSettlementStatusV1::NotDispatched => {
                    "not_dispatched"
                }
            };
            use astra_turn_types::ExplainAnalyzeAdmissionSettlementReasonV1 as Reason;
            let reason = match admission.reason {
                Reason::Accepted => "accepted",
                Reason::ClassifierUncertain => "classifier_uncertain",
                Reason::ClassifierConflicting => "classifier_conflicting",
                Reason::InvalidClassifierResponse => "invalid_classifier_response",
                Reason::ProviderRejected => "provider_rejected",
                Reason::PlanningRejected => "planning_rejected",
                Reason::ReconciliationRejected => "reconciliation_rejected",
                Reason::Unavailable { .. } => "unavailable",
                Reason::NotDispatched { .. } => "not_dispatched",
            };
            lines.push(format!(
                "{heading}{scope_label}: judgment admission={status} · {reason}\n"
            ));
        } else if detail
            .calls
            .iter()
            .any(|call| call.operation_id == "request_judgment")
        {
            lines.push(format!(
                "{heading}{scope_label}: judgment admission=not captured\n"
            ));
        }
    }
    if calls.is_empty() {
        if usage.available && usage.attempts.is_empty() && usage_snapshot_complete {
            lines.push(format!(
                "{heading}: provider attempts=0 (complete usage snapshot)\n"
            ));
        } else if usage.available && !usage.attempts.is_empty() {
            lines.push(format!(
                "{heading}: captured provider attempts={} · call timing unavailable\n",
                usage.attempts.len()
            ));
        } else {
            lines.push(format!("{heading}: provider attempt count unavailable\n"));
        }
    } else {
        for (scope, call) in &calls {
            let scope_label = if details.len() > 1 {
                format!(" scope[{scope}]")
            } else {
                String::new()
            };
            lines.push(format!(
                "{heading}{scope_label} call: {} {} · {}ms\n",
                call.operation_id,
                format!("{:?}", call.outcome).to_ascii_lowercase(),
                call.duration_ms
            ));
        }
    }

    if !scope_coverage_complete {
        let scope_reason = if graph.auxiliary_usage_unavailable() {
            "scope coverage incomplete (some scopes unavailable)"
        } else {
            "scope coverage incomplete"
        };
        lines.push(format!("{heading} usage: {scope_reason}\n"));
    }
    if call_timing_truncated {
        lines.push(format!(
            "{heading}: call timing capture incomplete (truncated)\n"
        ));
    } else if !call_timing_scope_complete && !calls.is_empty() {
        lines.push(format!(
            "{heading}: call timing scope coverage incomplete\n"
        ));
    }
    if usage.truncated {
        lines.push(format!(
            "{heading} usage: captured attempts are incomplete (truncated)\n"
        ));
    }
    if graph.auxiliary_capture_conflicted() || graph.auxiliary_usage_conflict_count() > 0 {
        lines.push(format!("{heading} usage: conflicting records\n"));
        return lines;
    }
    if !usage.available {
        let reason = if graph.auxiliary_usage_unavailable() {
            "one or more scopes unavailable"
        } else {
            "capture unavailable"
        };
        lines.push(format!("{heading} usage: {reason}\n"));
        return lines;
    }

    for attempt in usage.attempts {
        let status = match attempt.usage_status {
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact => "exact",
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial => "partial",
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable => "unavailable",
        };
        let bucket = |value: Option<u64>| {
            value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        };
        let (fresh, cache_read, cache_write, output) = attempt.usage.as_ref().map_or(
            (
                "unknown".to_owned(),
                "unknown".to_owned(),
                "unknown".to_owned(),
                "unknown".to_owned(),
            ),
            |usage| {
                (
                    bucket(usage.fresh_input_tokens),
                    bucket(usage.cache_read_tokens),
                    bucket(usage.cache_creation_tokens),
                    bucket(usage.output_tokens),
                )
            },
        );
        lines.push(format!(
            "{heading} usage: {} · {} · {} · fresh-in={} cache-read={} cache-write={} out={}\n",
            attempt.model_name,
            attempt.operation_id,
            status,
            fresh,
            cache_read,
            cache_write,
            output,
        ));
    }
    lines
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

    let primary_cache = report
        .runs
        .iter()
        .try_fold((0_u128, 0_u128), |(read, input), run| {
            let usage = run
                .outcome
                .explain_capture
                .as_ref()?
                .primary_prompt_cache_usage()?;
            Some((
                read + u128::from(usage.cache_read_tokens),
                input + u128::from(usage.checked_total_input_tokens()?),
            ))
        });
    let cache_ratio_pct = match primary_cache {
        Some((read, input)) if input > 0 => format!(
            "primary prompt-cache read={:.1}%",
            read as f64 / input as f64 * 100.0
        ),
        Some(_) => "primary prompt-cache read=n/a".to_owned(),
        None => "primary prompt-cache read=unknown (input coverage incomplete)".to_owned(),
    };
    s.push_str(&format!(
        "total={} passed={} failed={} cancelled={} unavailable={} | terminal-reported tokens: {} fresh-in/{}out cache-read={} cache-write={} (not full model cost) | {} | wall: {}m{}s (sum: {}m{}s)\n\n",
        report.total(),
        report.passed(),
        report.failed(),
        report.cancelled(),
        report.unavailable(),
        total_prompt,
        total_completion,
        total_cache_read,
        total_cache_create,
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
        for line in auxiliary_judgment_lines(run) {
            s.push_str(&line);
        }
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
        for error in &run.cleanup_errors {
            s.push_str(&format!("    cleanup: {error}\n"));
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
        let fallback_execution = run.session.as_ref().map(|cap| {
            s.push_str(&format!(
                "    session: id={} events={} skipped={} tools_from_journal={:?}\n",
                cap.session_id,
                cap.events.len(),
                cap.skipped_lines,
                cap.tools_invoked()
            ));
            let executions: Vec<_> = match (run.attempts.is_empty(), run.steps.is_empty()) {
                (true, true) => vec![&run.outcome],
                // An aggregate with steps cannot stand in for a missing root.
                (true, false) => Vec::new(),
                (false, _) => run
                    .attempts
                    .iter()
                    .map(|attempt| &attempt.outcome)
                    .chain(run.steps.iter().map(|step| &step.outcome))
                    .collect(),
            };
            let health = crate::pipeline_analysis::analyze_pipeline_health(cap, &executions);
            let prefix = health
                .stable_prefix_cache_coverage
                .map(|coverage| format!(" stable-prefix={:.0}%", coverage * 100.0))
                .unwrap_or_default();
            s.push_str(&format!(
                "    pipeline: feedback={} mean-primary-request-cache-read={}{} compactions={}\n",
                health.feedback_observations,
                health.cache_read_share_label(),
                prefix,
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
            health.execution
        });
        if let Some(execution) = run.execution.as_ref().or(fallback_execution.as_ref())
            && (execution.total_tool_calls > 0 || !execution.evidence_complete)
        {
            let scope = match execution.scope {
                crate::pipeline_analysis::ExecutionTraceScope::Session => "session",
                crate::pipeline_analysis::ExecutionTraceScope::CaseAttempts => "case_attempts",
            };
            s.push_str(&format!(
                "    execution: scope={scope} captures={}/{}\n",
                execution.captured_capture_count, execution.expected_capture_count,
            ));
            if !execution.evidence_complete {
                s.push_str(&format!(
                    "    execution: evidence=incomplete lower_bound=true skipped_lines={} dropped_lines={} integrity_errors={}\n",
                    execution.skipped_lines,
                    execution.dropped_lines,
                    execution.integrity_errors,
                ));
            }
            s.push_str(&format!(
                "    execution: tools={} executed={} success={} failed={} rejected={} reused={} suppressed={} deferred={} unknown={} unknown_disposition={}\n",
                execution.total_tool_calls,
                execution.executed_tool_calls,
                execution.successful_tool_calls,
                execution.failed_tool_calls,
                execution.rejected_tool_calls,
                execution.reused_tool_calls,
                execution.suppressed_tool_calls,
                execution.deferred_tool_calls,
                execution.unknown_outcome_tool_calls,
                execution.unknown_disposition_tool_calls,
            ));
            if execution.settlement_attempts > 0 {
                s.push_str(&format!(
                    "    execution: settlements={} success={} rejected={}\n",
                    execution.settlement_attempts,
                    execution.successful_settlements,
                    execution.rejected_settlements,
                ));
            }
            for (reason, count) in &execution.runtime_rejection_reasons {
                s.push_str(&format!(
                    "    execution: runtime_rejections={} × {}\n",
                    count, reason
                ));
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

    fn terminal_turn_mut(
        capture: &mut crate::explain_capture::ExplainCapture,
    ) -> &mut astra_turn_types::ExplainAnalyzeEventV1 {
        capture
            .events
            .iter_mut()
            .find(|event| {
                event.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::Turn
                    && event.transition == astra_turn_types::ExplainAnalyzeTransitionV1::Finished
            })
            .unwrap()
    }

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
            error_kind: None,
            explain_capture: None,
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
                session_captures: Vec::new(),
                execution: None,
                reproducer: None,
                digest: None,
                digest_error: None,
                failure_class: None,
                cleanup_errors: Vec::new(),
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
    fn pipeline_cache_display_preserves_unknown_zero_and_physical_execution_coverage() {
        let mut report = mk_report_passed();
        report.runs[0].session = Some(SessionCapture {
            session_id: "cache-session".into(),
            journal_path: "capture.jsonl".into(),
            events: vec![],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        });
        let known = crate::exec::test_support::cache_request_outcome("r", "t", &[(100, 900, 0)]);
        report.runs[0].outcome = known.clone();
        assert!(
            render(&report, Format::Text, false).contains("mean-primary-request-cache-read=90.0%")
        );
        report.runs[0].outcome =
            crate::exec::test_support::cache_request_outcome("r", "t", &[(0, 0, 0)]);
        assert!(
            render(&report, Format::Text, false)
                .contains("mean-primary-request-cache-read=n/a (zero input)")
        );
        // An already-present unknown attempt cannot borrow the aggregate's usage.
        report.runs[0].outcome = known.clone();
        report.runs[0].attempts = vec![AttemptRecord {
            attempt_index: 0,
            outcome: mk_outcome(),
        }];
        assert!(
            render(&report, Format::Text, false)
                .contains("mean-primary-request-cache-read=unknown")
        );
        report.runs[0].attempts[0].outcome = known;
        report.runs[0].steps.push(StepResult {
            step_index: 0,
            prompt: "continue".into(),
            duration_ms: 0,
            criteria: vec![],
            passed: true,
            outcome: crate::exec::test_support::cache_request_outcome(
                "next",
                "t2",
                &[(900, 100, 0)],
            ),
        });
        assert!(
            render(&report, Format::Text, false).contains("mean-primary-request-cache-read=50.0%")
        );
        report.runs[0].attempts.clear();
        assert!(
            render(&report, Format::Text, false)
                .contains("mean-primary-request-cache-read=unknown")
        );
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
    fn text_report_shows_incomplete_execution_without_selected_session() {
        let mut report = mk_report_passed();
        report.runs[0].execution = Some(crate::pipeline_analysis::ExecutionTraceReport {
            scope: crate::pipeline_analysis::ExecutionTraceScope::CaseAttempts,
            expected_capture_count: 2,
            captured_capture_count: 1,
            total_tool_calls: 1,
            executed_tool_calls: 1,
            successful_tool_calls: 1,
            evidence_complete: false,
            ..Default::default()
        });

        let out = render(&report, Format::Text, false);
        assert!(out.contains("execution: scope=case_attempts captures=1/2"));
        assert!(out.contains("execution: evidence=incomplete"));
        assert!(out.contains("execution: tools=1 executed=1 success=1"));
    }

    #[test]
    fn json_report_roundtrips() {
        let mut r = mk_report_passed();
        r.runs[0].session_captures = vec![
            SessionCapture {
                session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
                ..Default::default()
            },
            SessionCapture {
                session_id: "550e8400-e29b-41d4-a716-446655440001".into(),
                ..Default::default()
            },
        ];
        let out = render(&r, Format::Json, false);
        let parsed: SuiteReport = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.total(), 1);
        assert_eq!(parsed.passed(), 1);
        let captures = &parsed.runs[0].session_captures;
        assert_eq!(captures.len(), 2);
        assert_eq!(
            captures[0].session_id,
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            captures[1].session_id,
            "550e8400-e29b-41d4-a716-446655440001"
        );
    }

    #[test]
    fn json_report_includes_execution_attribution_when_captured() {
        let mut report = mk_report_passed();
        report.runs[0].execution = Some(crate::pipeline_analysis::ExecutionTraceReport {
            total_tool_calls: 5,
            executed_tool_calls: 3,
            successful_tool_calls: 2,
            failed_tool_calls: 1,
            rejected_tool_calls: 1,
            suppressed_tool_calls: 1,
            settlement_attempts: 2,
            successful_settlements: 1,
            rejected_settlements: 1,
            runtime_rejection_reasons: [("work_settlement_evidence_required".into(), 1)]
                .into_iter()
                .collect(),
            ..Default::default()
        });

        let json: serde_json::Value =
            serde_json::from_str(&render(&report, Format::Json, false)).unwrap();
        assert_eq!(json["runs"][0]["execution"]["total_tool_calls"], 5);
        assert_eq!(json["runs"][0]["execution"]["executed_tool_calls"], 3);
        assert_eq!(json["runs"][0]["execution"]["rejected_tool_calls"], 1);
        assert_eq!(json["runs"][0]["execution"]["suppressed_tool_calls"], 1);
        assert_eq!(
            json["runs"][0]["execution"]["runtime_rejection_reasons"]["work_settlement_evidence_required"],
            1
        );
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
            session_captures: Vec::new(),
            execution: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            cleanup_errors: Vec::new(),
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
                cleanup_errors: Vec::new(),
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                session_captures: Vec::new(),
                execution: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };
        let out = render_text(&r, false);
        assert!(
            out.contains("terminal-reported tokens: 0 fresh-in/0out cache-read=0 cache-write=0 (not full model cost)"),
            "missing token summary: {out}"
        );
        assert!(out.contains("wall: 0m5s"), "missing wall time: {out}");
    }

    #[test]
    fn render_text_keeps_terminal_cache_counts_without_claiming_primary_share() {
        let mut r = SuiteReport {
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
                cleanup_errors: Vec::new(),
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                session_captures: Vec::new(),
                execution: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        assert!(
            out.contains(
                "terminal-reported tokens: 200 fresh-in/50out cache-read=800 cache-write=0 (not full model cost)"
            ),
            "reported cache counts must remain visible: {out}"
        );
        assert!(out.contains("primary prompt-cache read=unknown"));
        let mut capture = crate::explain_capture::ExplainCapture::default();
        for (node, kind) in [("turn", "turn"), ("request", "provider_attempt")] {
            let mut start = serde_json::json!({
                "schema_version":1,"event_id":format!("{node}-start"),"run_id":"r",
                "turn_id":"t","node_id":node,"producer_id":"p","clock_domain_id":"c",
                "kind":kind,"label":node,"transition":"started","elapsed_ms":0
            });
            if node == "request" {
                start["round_index"] = serde_json::json!(0);
                start["attempt_index"] = serde_json::json!(0);
            }
            let mut finish = start.clone();
            finish["event_id"] = serde_json::json!(format!("{node}-finish"));
            finish["transition"] = serde_json::json!("finished");
            finish["outcome"] = serde_json::json!("completed");
            finish["start_elapsed_ms"] = serde_json::json!(0);
            finish["duration_ms"] = serde_json::json!(0);
            if node == "request" {
                finish["usage"] = serde_json::json!({"basis":"provider_exact",
                    "fresh_input_tokens":100,"cache_read_tokens":900,"cache_creation_tokens":0});
            } else {
                finish["auxiliary_usage"] = serde_json::json!({"available":true,"attempts":[{
                    "attempt_id":"aux","usage_status":"provider_exact","provider":"typesafe",
                    "offering_id":"o","model_name":"jev","purpose":"introspection","operation_id":"request_judgment",
                    "usage":{"basis":"provider_exact","fresh_input_tokens":999999,"output_tokens":100}
                }]});
                finish["auxiliary_details"] = serde_json::json!({"calls":[{
                    "call_id":"request_judgment:initial:0","operation_id":"request_judgment",
                    "stage":"initial","start_elapsed_ms":0,"duration_ms":0,"outcome":"succeeded"
                }]});
            }
            for fact in [start, finish] {
                let fact: astra_turn_types::ExplainAnalyzeEventV1 =
                    serde_json::from_value(fact).unwrap();
                assert!(fact.is_valid());
                capture.events.push(fact);
            }
        }
        capture.bind(Some("r"));
        r.runs[0].outcome.explain_capture = Some(capture);
        let rendered = render_text(&r, false);
        assert!(rendered.contains("primary prompt-cache read=90.0%"));
        assert!(rendered.contains("auxiliary call: request_judgment succeeded · 0ms"));
        assert!(rendered.contains(
            "auxiliary usage: jev · request_judgment · exact · fresh-in=999999 cache-read=unknown cache-write=unknown out=100"
        ));

        let mut failed_auxiliary = r.clone();
        let capture = failed_auxiliary.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let terminal = terminal_turn_mut(capture);
        terminal.auxiliary_usage = Some(Box::new(
            serde_json::from_value(serde_json::json!({"available":true,"attempts":[{
                "attempt_id":"aux","usage_status":"unavailable","provider":"typesafe",
                "offering_id":"o","model_name":"jev-1.13.0","purpose":"introspection",
                "operation_id":"request_judgment"
            }]}))
            .unwrap(),
        ));
        terminal.auxiliary_details.as_mut().unwrap().calls[0].outcome =
            astra_turn_types::ExplainAnalyzeOutcomeV1::Failed;
        let rendered = render_text(&failed_auxiliary, false);
        assert!(rendered.contains("auxiliary call: request_judgment failed · 0ms"));
        assert!(rendered.contains(
            "auxiliary usage: jev-1.13.0 · request_judgment · unavailable · fresh-in=unknown cache-read=unknown cache-write=unknown out=unknown"
        ));

        let mut no_auxiliary = r.clone();
        let capture = no_auxiliary.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let terminal = terminal_turn_mut(capture);
        terminal.auxiliary_usage = Some(Box::new(
            serde_json::from_value(serde_json::json!({"available":true,"attempts":[]})).unwrap(),
        ));
        terminal.auxiliary_details = Some(Box::new(
            serde_json::from_value(serde_json::json!({"calls":[],"truncated":true})).unwrap(),
        ));
        let rendered = render_text(&no_auxiliary, false);
        assert!(rendered.contains("auxiliary: provider attempts=0 (complete usage snapshot)"));
        assert!(rendered.contains("call timing capture incomplete (truncated)"));
        assert!(!rendered.contains("call timing=none"));

        let mut admission_does_not_hide_attempt = r.clone();
        let capture = admission_does_not_hide_attempt.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let terminal = terminal_turn_mut(capture);
        let details = terminal.auxiliary_details.as_mut().unwrap();
        details.admission = Some(
            serde_json::from_value(serde_json::json!({
                "status":"rejected","reason":{"kind":"classifier_uncertain"}
            }))
            .unwrap(),
        );
        let rendered = render_text(&admission_does_not_hide_attempt, false);
        assert!(rendered.contains("auxiliary: judgment admission=rejected · classifier_uncertain"));
        assert!(rendered.contains("auxiliary call: request_judgment succeeded · 0ms"));
        terminal_turn_mut(
            admission_does_not_hide_attempt.runs[0]
                .outcome
                .explain_capture
                .as_mut()
                .unwrap(),
        )
        .auxiliary_details
        .as_mut()
        .unwrap()
        .calls
        .clear();
        let rendered = render_text(&admission_does_not_hide_attempt, false);
        assert!(rendered.contains("auxiliary: judgment admission=rejected · classifier_uncertain"));
        assert!(rendered.contains("captured provider attempts=1 · call timing unavailable"));
        assert!(!rendered.contains("no provider call"));

        let mut unavailable_admission = r.clone();
        terminal_turn_mut(
            unavailable_admission.runs[0]
                .outcome
                .explain_capture
                .as_mut()
                .unwrap(),
        )
        .auxiliary_details
        .as_mut()
        .unwrap()
        .admission = Some(
            serde_json::from_value(serde_json::json!({
                "status":"unavailable","reason":{"kind":"unavailable","reason":"execution_error"}
            }))
            .unwrap(),
        );
        let rendered = render_text(&unavailable_admission, false);
        assert!(rendered.contains("auxiliary: judgment admission=unavailable · unavailable"));
        assert!(rendered.contains("auxiliary call: request_judgment succeeded · 0ms"));

        let mut multiple_scopes = r.clone();
        let capture = multiple_scopes.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let first = terminal_turn_mut(capture);
        first.auxiliary_details.as_mut().unwrap().admission = Some(
            serde_json::from_value(serde_json::json!({
                "status":"rejected","reason":{"kind":"classifier_uncertain"}
            }))
            .unwrap(),
        );
        let mut second = first.clone();
        second.event_id = "turn-2-finish".into();
        second.turn_id = "t2".into();
        second.node_id = "turn-2".into();
        second.clock_domain_id = "c2".into();
        let second_details = second.auxiliary_details.as_mut().unwrap();
        second_details.admission = Some(
            serde_json::from_value(serde_json::json!({
                "status":"accepted","reason":{"kind":"accepted"},
                "decision":{"result":"decided","classification":{
                    "work_required":false,"activation_deferred":false,"domain":null,
                    "mutation":"read_only","scope":"unknown",
                    "parallel_subruns":false,"capabilities":[]
                }}
            }))
            .unwrap(),
        );
        second_details.calls[0].duration_ms = 12;
        second.elapsed_ms = 12;
        second.duration_ms = Some(12);
        let mut second_start = capture
            .events
            .iter()
            .find(|event| {
                event.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::Turn
                    && event.transition == astra_turn_types::ExplainAnalyzeTransitionV1::Started
            })
            .unwrap()
            .clone();
        second_start.event_id = "turn-2-start".into();
        second_start.turn_id = "t2".into();
        second_start.node_id = "turn-2".into();
        second_start.clock_domain_id = "c2".into();
        capture.events.push(second_start);
        assert!(second.is_valid());
        capture.events.push(second);
        let rendered = render_text(&multiple_scopes, false);
        assert!(
            rendered
                .contains("auxiliary scope[0]: judgment admission=rejected · classifier_uncertain"),
            "{rendered}"
        );
        assert!(rendered.contains("auxiliary scope[0] call: request_judgment succeeded · 0ms"));
        assert!(rendered.contains("auxiliary scope[1]: judgment admission=accepted · accepted"));
        assert!(rendered.contains("auxiliary scope[1] call: request_judgment succeeded · 12ms"));

        let missing_admission = render_text(&r, false);
        assert!(missing_admission.contains("auxiliary: judgment admission=not captured"));

        let mut truncated_timing = r.clone();
        let capture = truncated_timing.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        terminal_turn_mut(capture)
            .auxiliary_details
            .as_mut()
            .unwrap()
            .truncated = true;
        assert!(
            render_text(&truncated_timing, false)
                .contains("call timing capture incomplete (truncated)")
        );

        let mut partial_scope = r.clone();
        let capture = partial_scope.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let mut unavailable_scope = terminal_turn_mut(capture).clone();
        unavailable_scope.event_id = "turn-2-finish".into();
        unavailable_scope.turn_id = "turn-2".into();
        unavailable_scope.node_id = "turn-2".into();
        unavailable_scope.clock_domain_id = "clock-2".into();
        unavailable_scope.auxiliary_usage = Some(Box::new(
            serde_json::from_value(serde_json::json!({"available":false,"attempts":[]})).unwrap(),
        ));
        unavailable_scope.auxiliary_details = None;
        capture.events.push(unavailable_scope);
        let rendered = render_text(&partial_scope, false);
        assert!(rendered.contains("scope coverage incomplete (some scopes unavailable)"));

        let mut conflicted_turn = r.clone();
        let capture = conflicted_turn.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap();
        let mut conflicting = terminal_turn_mut(capture).clone();
        conflicting.event_id = "turn-conflict".into();
        conflicting.auxiliary_usage = Some(Box::new(
            serde_json::from_value(serde_json::json!({"available":false,"attempts":[]})).unwrap(),
        ));
        capture.events.push(conflicting);
        let rendered = render_text(&conflicted_turn, false);
        assert!(rendered.contains("auxiliary usage: conflicting records"));
        assert!(!rendered.contains("auxiliary usage: capture unavailable"));
        let complete = r.clone();
        let mut missing_run = complete.runs[0].clone();
        missing_run.outcome.explain_capture = None;
        let mut partial_suite = complete.clone();
        partial_suite.runs.push(missing_run);
        assert!(render_text(&partial_suite, false).contains("primary prompt-cache read=unknown"));
        for gate in 0..3 {
            let mut degraded = complete.clone();
            let capture = degraded.runs[0].outcome.explain_capture.as_mut().unwrap();
            match gate {
                0 => capture.identity_verified = false,
                1 => capture.gap_unrecovered = true,
                _ => capture.diagnostics.push("delivery_degraded".into()),
            }
            assert!(render_text(&degraded, false).contains("primary prompt-cache read=unknown"));
        }
        r.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap()
            .snapshot_pending = true;
        assert!(render_text(&r, false).contains("primary prompt-cache read=unknown"));
        let capture = r.runs[0].outcome.explain_capture.as_mut().unwrap();
        capture.snapshot_pending = false;
        let usage = capture
            .events
            .iter_mut()
            .find_map(|event| event.usage.as_mut())
            .unwrap();
        usage.fresh_input_tokens = Some(0);
        usage.cache_read_tokens = Some(0);
        assert!(render_text(&r, false).contains("primary prompt-cache read=n/a"));
        let usage = r.runs[0]
            .outcome
            .explain_capture
            .as_mut()
            .unwrap()
            .events
            .iter_mut()
            .find_map(|event| event.usage.as_mut())
            .unwrap();
        usage.fresh_input_tokens = Some(u64::MAX);
        usage.cache_read_tokens = Some(1);
        assert!(render_text(&r, false).contains("primary prompt-cache read=unknown"));
    }

    #[test]
    fn render_text_preserves_reported_cache_creation_without_input_coverage() {
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
                cleanup_errors: Vec::new(),
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                session_captures: Vec::new(),
                execution: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        assert!(
            out.contains("cache-read=200 cache-write=200")
                && out.contains("primary prompt-cache read=unknown"),
            "mixed terminal counters cannot certify primary cache share: {out}"
        );
    }

    #[test]
    fn render_text_does_not_certify_zero_cache_from_terminal_counters() {
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
                cleanup_errors: Vec::new(),
                has_warnings: false,
                attempts: Vec::new(),
                session: None,
                session_captures: Vec::new(),
                execution: None,
                reproducer: None,
                digest: None,
                digest_error: None,
            }],
            ..Default::default()
        };

        let out = render_text(&r, false);
        assert!(
            out.contains("cache-read=0 cache-write=200")
                && out.contains("primary prompt-cache read=unknown"),
            "retain reported counts without inventing primary coverage: {out}"
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
            cleanup_errors: Vec::new(),
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            session_captures: Vec::new(),
            execution: None,
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
            cleanup_errors: Vec::new(),
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            session_captures: Vec::new(),
            execution: None,
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
            cleanup_errors: Vec::new(),
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            session_captures: Vec::new(),
            execution: None,
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
            cleanup_errors: Vec::new(),
            has_warnings: false,
            attempts: Vec::new(),
            session: None,
            session_captures: Vec::new(),
            execution: None,
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
