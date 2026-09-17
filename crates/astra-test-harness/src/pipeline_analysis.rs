//! Pipeline session analysis — extracts context pipeline health from journal events.
//!
//! Reads canonical `pipeline_feedback` / `pipeline_alert` /
//! `pipeline_compaction_audit` journal events and `CompactionFired` step events from a
//! SessionCapture and produces structured diagnostics: cache trend, compaction
//! frequency, pressure evolution, and alert timeline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_capture::SessionCapture;

const RAW_CACHE_BREAK_MIN_RATIO: f64 = 0.25;

/// Aggregate pipeline health metrics for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineHealthReport {
    /// Per-turn billable cache-read share (0.0–1.0). This uses the complete
    /// provider input accounting denominator, so it is not the same as
    /// stable-prefix coverage when conversation history is uncached.
    pub cache_hit_ratios: Vec<f64>,
    /// Average billable cache-read share across all turns.
    pub avg_cache_hit_ratio: f64,
    /// Sum of the runtime's stable provider-prefix estimates for feedback
    /// observations with actual request usage and a `provider-prefix-v1`
    /// identity.
    #[serde(default)]
    pub stable_prefix_cache_eligible_tokens: u64,
    /// Number of cache observations with an eligible-prefix estimate and
    /// provider usage. This includes layouts that cannot be certified as a
    /// contiguous provider prefix.
    #[serde(default)]
    pub stable_prefix_cache_observations: u32,
    /// Number of observations whose typed identity declares the contiguous
    /// `provider-prefix-v1` layout used by stable-prefix coverage.
    #[serde(default)]
    pub provider_prefix_cache_observations: u32,
    /// Cache-read tokens attributable to the stable prefix under the
    /// `provider-prefix-v1` layout. Reads are capped at the corresponding
    /// eligible prefix so conversation-history caching cannot inflate this
    /// diagnostic above 100%.
    #[serde(default)]
    pub stable_prefix_cache_read_tokens: u64,
    /// Token-weighted stable-prefix coverage. `None` means no supported
    /// provider-prefix observation exposed the typed estimate; it must not be
    /// rendered as a zero-hit cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_prefix_cache_coverage: Option<f64>,
    /// Number of compaction events recorded.
    pub compaction_count: u32,
    /// Total tokens freed by compaction.
    pub total_tokens_freed: u64,
    /// Alerts that fired (turn, rule, severity).
    pub alerts: Vec<PipelineAlertEntry>,
    /// Number of explicit prompt-cache break alerts.
    pub prompt_cache_breaks: u32,
    /// Whether a compaction cascade was detected.
    pub cascade_detected: bool,
    /// Number of turns with pipeline feedback.
    pub turns_with_feedback: u32,
    /// Number of pipeline events with invalid typed payloads. Any non-zero
    /// value means the measured health report is incomplete and cannot certify
    /// cache/alert criteria.
    pub invalid_events: u32,
    /// Execution facts derived from the same durable journal. These counters
    /// keep a correctly rejected model action distinct from a successful
    /// state transition, so efficiency failures cannot be mistaken for
    /// runtime correctness failures.
    #[serde(default)]
    pub execution: ExecutionTraceReport,
}

/// Bounded, typed execution counters for one captured invocation.
///
/// This is deliberately a diagnostic projection: it never changes criterion
/// truth and does not infer semantic task success from tool names or prose.
/// `total_tool_calls` includes every canonical audit record, while the
/// disposition buckets make it explicit which records reached an executor.
/// In particular, suppressed/reused placeholders cannot inflate successful
/// execution or settlement counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTraceReport {
    /// The journal scope represented by these counters. A single session is
    /// only the final/root session selected for a case; `case_attempts`
    /// combines every captured root-attempt session.
    #[serde(default)]
    pub scope: ExecutionTraceScope,
    /// Number of root-attempt sessions expected by the caller.
    #[serde(default)]
    pub expected_capture_count: u32,
    /// Number of root-attempt sessions actually present in the projection.
    #[serde(default)]
    pub captured_capture_count: u32,
    /// Every de-duplicated tool record in the invocation-scoped journal,
    /// including records that were rejected or intentionally suppressed.
    pub total_tool_calls: u32,
    /// Records whose canonical disposition was `executed`.
    #[serde(default)]
    pub executed_tool_calls: u32,
    /// Executed records with an explicit successful outcome.
    pub successful_tool_calls: u32,
    /// Executed records with an explicit failed outcome.
    pub failed_tool_calls: u32,
    /// Records rejected before execution by the runtime.
    #[serde(default)]
    pub rejected_tool_calls: u32,
    /// Records whose result was reused without executing a new request.
    #[serde(default)]
    pub reused_tool_calls: u32,
    /// Audit-only records intentionally omitted by routing or deduplication.
    #[serde(default)]
    pub suppressed_tool_calls: u32,
    /// Records deferred for a later activation or retry opportunity.
    #[serde(default)]
    pub deferred_tool_calls: u32,
    /// Executed records whose outcome was not present in the journal.
    pub unknown_outcome_tool_calls: u32,
    /// Records carrying a non-null disposition that the current reader does
    /// not understand. They are excluded from execution buckets and make the
    /// projection incomplete instead of being guessed as successes.
    #[serde(default)]
    pub unknown_disposition_tool_calls: u32,
    /// Every `settle_work_item` request, including rejected or suppressed
    /// requests. Successful transitions are counted separately below.
    pub settlement_attempts: u32,
    /// Settlement records that both executed and returned `ok=true`.
    pub successful_settlements: u32,
    /// Settlement records rejected before execution.
    pub rejected_settlements: u32,
    #[serde(default)]
    pub runtime_rejection_reasons: BTreeMap<String, u32>,
    /// False when the journal capture dropped or skipped rows, or detected an
    /// integrity conflict. Counters are then lower bounds (or unavailable)
    /// and must not be read as a complete execution trace.
    #[serde(default = "default_evidence_complete")]
    pub evidence_complete: bool,
    #[serde(default)]
    pub skipped_lines: u32,
    #[serde(default)]
    pub dropped_lines: u32,
    #[serde(default)]
    pub integrity_errors: u32,
}

/// Scope of an execution attribution projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTraceScope {
    /// Counters for one selected session journal.
    #[default]
    Session,
    /// Counters aggregated across every root attempt captured for a case.
    CaseAttempts,
}

fn default_evidence_complete() -> bool {
    true
}

impl Default for ExecutionTraceReport {
    fn default() -> Self {
        Self {
            scope: ExecutionTraceScope::Session,
            expected_capture_count: 0,
            captured_capture_count: 0,
            total_tool_calls: 0,
            executed_tool_calls: 0,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            rejected_tool_calls: 0,
            reused_tool_calls: 0,
            suppressed_tool_calls: 0,
            deferred_tool_calls: 0,
            unknown_outcome_tool_calls: 0,
            unknown_disposition_tool_calls: 0,
            settlement_attempts: 0,
            successful_settlements: 0,
            rejected_settlements: 0,
            runtime_rejection_reasons: BTreeMap::new(),
            evidence_complete: true,
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineAlertEntry {
    pub turn: u32,
    pub rule: String,
    pub severity: String,
}

/// Analyze a session capture for pipeline health.
pub fn analyze_pipeline_health(capture: &SessionCapture) -> PipelineHealthReport {
    let mut report = PipelineHealthReport::default();
    let mut feedback_ratios = Vec::new();
    let mut raw_usage_ratios = Vec::new();

    for event in &capture.events {
        let metadata = event.raw.get("metadata");

        match event.event_type.as_str() {
            "pipeline_feedback" => {
                let Some(metadata) = metadata else {
                    report.invalid_events = report.invalid_events.saturating_add(1);
                    continue;
                };
                let Ok(feedback_event) = serde_json::from_value::<
                    astra_turn_core::pipeline_journal::PipelineJournalEvent,
                >(metadata.clone()) else {
                    report.invalid_events = report.invalid_events.saturating_add(1);
                    continue;
                };
                let Some(frame) = feedback_event.runtime_feedback else {
                    report.invalid_events = report.invalid_events.saturating_add(1);
                    continue;
                };
                let event_turn = event.raw.get("turn").and_then(Value::as_u64);
                if feedback_event.kind
                    != astra_turn_core::pipeline_journal::PipelineEventKind::Feedback
                    || feedback_event.turn != frame.progress.session_turn
                    || event_turn != Some(u64::from(frame.progress.session_turn))
                    || !frame.is_valid()
                {
                    report.invalid_events = report.invalid_events.saturating_add(1);
                    continue;
                }
                let Some(ratio) = frame.cache_hit_ratio() else {
                    report.invalid_events = report.invalid_events.saturating_add(1);
                    continue;
                };
                feedback_ratios.push(ratio);
                if let (Some(eligible), Some(usage)) = (
                    frame.context.estimated_cache_eligible_tokens,
                    frame.request_usage.as_ref(),
                ) && eligible > 0
                {
                    report.stable_prefix_cache_observations =
                        report.stable_prefix_cache_observations.saturating_add(1);
                    if frame
                        .context
                        .prompt_cache_identity
                        .as_ref()
                        .is_some_and(|identity| identity.cache_layout == "provider-prefix-v1")
                    {
                        report.provider_prefix_cache_observations =
                            report.provider_prefix_cache_observations.saturating_add(1);
                        report.stable_prefix_cache_eligible_tokens = report
                            .stable_prefix_cache_eligible_tokens
                            .saturating_add(eligible);
                        report.stable_prefix_cache_read_tokens = report
                            .stable_prefix_cache_read_tokens
                            .saturating_add(usage.cache_read.min(eligible));
                    }
                }
            }
            "llm_response_full" => {
                if let Some(ratio) = raw_llm_response_cache_hit_ratio(event) {
                    raw_usage_ratios.push(ratio);
                }
            }
            "pipeline_compaction_audit" => {
                if let Some(meta) = metadata {
                    report.compaction_count += 1;
                    if let Some(freed) = meta.get("tokens_freed").and_then(|v| v.as_u64()) {
                        report.total_tokens_freed += freed;
                    }
                }
            }
            "CompactionFired" => {
                if let Some(payload) = event.raw.get("payload") {
                    report.compaction_count += 1;
                    if let Some(saved) = payload.get("tokens_saved").and_then(Value::as_u64) {
                        report.total_tokens_freed += saved;
                    }
                }
            }
            "pipeline_alert" => {
                if let Some(meta) = metadata {
                    let rule = meta
                        .get("alert_rule")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let severity = meta
                        .get("alert_severity")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let turn = event.raw.get("turn").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

                    if rule == "compaction_cascade" {
                        report.cascade_detected = true;
                    }
                    if rule == "prompt_cache_break" {
                        report.prompt_cache_breaks += 1;
                    }

                    report.alerts.push(PipelineAlertEntry {
                        turn,
                        rule,
                        severity,
                    });
                }
            }
            _ => {}
        }
    }

    if report.prompt_cache_breaks == 0 {
        let raw_breaks = detect_raw_prompt_cache_breaks(capture);
        report.prompt_cache_breaks = raw_breaks.len() as u32;
        report.alerts.extend(raw_breaks);
    }

    report.cache_hit_ratios = if feedback_ratios.is_empty() {
        raw_usage_ratios
    } else {
        feedback_ratios
    };
    report.turns_with_feedback = report.cache_hit_ratios.len() as u32;
    if !report.cache_hit_ratios.is_empty() {
        report.avg_cache_hit_ratio =
            report.cache_hit_ratios.iter().sum::<f64>() / report.cache_hit_ratios.len() as f64;
    }
    if report.stable_prefix_cache_eligible_tokens > 0 {
        report.stable_prefix_cache_coverage = Some(
            report.stable_prefix_cache_read_tokens as f64
                / report.stable_prefix_cache_eligible_tokens as f64,
        );
    }

    report.execution = analyze_execution_trace(capture);

    report
}

/// Analyze complete canonical tool records without interpreting assistant
/// prose. Runtime rejection reasons prefer the typed result payload because it
/// carries the actionable contract code (for example
/// `work_settlement_evidence_required`) rather than a broad journal category.
pub fn analyze_execution_trace(capture: &SessionCapture) -> ExecutionTraceReport {
    let has_integrity_errors = capture.has_integrity_errors();
    if capture.events.is_empty()
        && capture.skipped_lines == 0
        && capture.dropped_lines == 0
        && !has_integrity_errors
    {
        return ExecutionTraceReport::default();
    }
    let mut report = ExecutionTraceReport {
        scope: ExecutionTraceScope::Session,
        expected_capture_count: 1,
        captured_capture_count: 1,
        evidence_complete: capture.skipped_lines == 0
            && capture.dropped_lines == 0
            && !has_integrity_errors,
        skipped_lines: capture.skipped_lines,
        dropped_lines: capture.dropped_lines,
        integrity_errors: capture.integrity_errors.max(u32::from(
            has_integrity_errors && capture.integrity_errors == 0,
        )),
        ..ExecutionTraceReport::default()
    };
    for call in capture.journal_tool_calls() {
        report.total_tool_calls = report.total_tool_calls.saturating_add(1);
        if call.name == "settle_work_item" {
            report.settlement_attempts = report.settlement_attempts.saturating_add(1);
        }
        let disposition = match journal_tool_call_disposition(&call) {
            Ok(disposition) => disposition,
            Err(()) => {
                report.unknown_disposition_tool_calls =
                    report.unknown_disposition_tool_calls.saturating_add(1);
                report.evidence_complete = false;
                continue;
            }
        };
        match disposition {
            astra_services::session_journal::ToolCallDisposition::Executed => {
                report.executed_tool_calls = report.executed_tool_calls.saturating_add(1);
                match call.ok {
                    Some(true) => {
                        report.successful_tool_calls =
                            report.successful_tool_calls.saturating_add(1)
                    }
                    Some(false) => {
                        report.failed_tool_calls = report.failed_tool_calls.saturating_add(1)
                    }
                    None => {
                        report.unknown_outcome_tool_calls =
                            report.unknown_outcome_tool_calls.saturating_add(1)
                    }
                }
            }
            astra_services::session_journal::ToolCallDisposition::Rejected => {
                report.rejected_tool_calls = report.rejected_tool_calls.saturating_add(1);
            }
            astra_services::session_journal::ToolCallDisposition::Reused => {
                report.reused_tool_calls = report.reused_tool_calls.saturating_add(1);
            }
            astra_services::session_journal::ToolCallDisposition::Suppressed => {
                report.suppressed_tool_calls = report.suppressed_tool_calls.saturating_add(1);
            }
            astra_services::session_journal::ToolCallDisposition::Deferred => {
                report.deferred_tool_calls = report.deferred_tool_calls.saturating_add(1);
            }
        }

        if call.name == "settle_work_item"
            && disposition == astra_services::session_journal::ToolCallDisposition::Executed
            && call.ok == Some(true)
        {
            report.successful_settlements = report.successful_settlements.saturating_add(1);
        }

        if disposition == astra_services::session_journal::ToolCallDisposition::Rejected {
            if call.name == "settle_work_item" {
                report.rejected_settlements = report.rejected_settlements.saturating_add(1);
            }
            let reason = call
                .result
                .as_ref()
                .and_then(|result| result.get("error_kind"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    call.runtime_metadata
                        .get("error_kind")
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("unknown")
                .to_string();
            *report.runtime_rejection_reasons.entry(reason).or_default() += 1;
        }
    }
    report
}

/// Aggregate execution attribution across the root-attempt sessions owned by
/// one case. Each session is already de-duplicated by `journal_tool_calls`;
/// duplicate session identities are rejected rather than counted twice.
/// Missing captures are lower-bound evidence and therefore cannot certify the
/// complete case projection.
pub fn analyze_execution_traces<'a, I>(
    captures: I,
    expected_capture_count: usize,
) -> ExecutionTraceReport
where
    I: IntoIterator<Item = &'a SessionCapture>,
{
    let mut report = ExecutionTraceReport {
        scope: ExecutionTraceScope::CaseAttempts,
        expected_capture_count: expected_capture_count.min(u32::MAX as usize) as u32,
        captured_capture_count: 0,
        evidence_complete: true,
        ..ExecutionTraceReport::default()
    };
    let mut seen_sessions = std::collections::BTreeSet::new();
    for capture in captures {
        report.captured_capture_count = report.captured_capture_count.saturating_add(1);
        if capture.session_id.trim().is_empty() || !seen_sessions.insert(capture.session_id.clone())
        {
            report.evidence_complete = false;
            continue;
        }
        let session = analyze_execution_trace(capture);
        report.total_tool_calls = report
            .total_tool_calls
            .saturating_add(session.total_tool_calls);
        report.executed_tool_calls = report
            .executed_tool_calls
            .saturating_add(session.executed_tool_calls);
        report.successful_tool_calls = report
            .successful_tool_calls
            .saturating_add(session.successful_tool_calls);
        report.failed_tool_calls = report
            .failed_tool_calls
            .saturating_add(session.failed_tool_calls);
        report.rejected_tool_calls = report
            .rejected_tool_calls
            .saturating_add(session.rejected_tool_calls);
        report.reused_tool_calls = report
            .reused_tool_calls
            .saturating_add(session.reused_tool_calls);
        report.suppressed_tool_calls = report
            .suppressed_tool_calls
            .saturating_add(session.suppressed_tool_calls);
        report.deferred_tool_calls = report
            .deferred_tool_calls
            .saturating_add(session.deferred_tool_calls);
        report.unknown_outcome_tool_calls = report
            .unknown_outcome_tool_calls
            .saturating_add(session.unknown_outcome_tool_calls);
        report.unknown_disposition_tool_calls = report
            .unknown_disposition_tool_calls
            .saturating_add(session.unknown_disposition_tool_calls);
        report.settlement_attempts = report
            .settlement_attempts
            .saturating_add(session.settlement_attempts);
        report.successful_settlements = report
            .successful_settlements
            .saturating_add(session.successful_settlements);
        report.rejected_settlements = report
            .rejected_settlements
            .saturating_add(session.rejected_settlements);
        for (reason, count) in session.runtime_rejection_reasons {
            report
                .runtime_rejection_reasons
                .entry(reason)
                .and_modify(|existing| *existing = existing.saturating_add(count))
                .or_insert(count);
        }
        report.evidence_complete &= session.evidence_complete;
        report.skipped_lines = report.skipped_lines.saturating_add(session.skipped_lines);
        report.dropped_lines = report.dropped_lines.saturating_add(session.dropped_lines);
        report.integrity_errors = report
            .integrity_errors
            .saturating_add(session.integrity_errors);
    }
    report.evidence_complete &= report.captured_capture_count as usize == expected_capture_count;
    report
}

/// Parse the canonical producer disposition carried in the bounded journal
/// projection. Older records omitted this field, so a missing/null value is
/// classified with the same legacy markers as
/// `ToolCallRecord::effective_disposition`; an explicit disposition always
/// wins over the `ok` bit.
fn journal_tool_call_disposition(
    call: &crate::session_capture::JournalToolCall,
) -> Result<astra_services::session_journal::ToolCallDisposition, ()> {
    let metadata = &call.runtime_metadata;
    if let Some(value) = metadata.get("disposition").filter(|value| !value.is_null()) {
        // An explicit value is authoritative. Never silently reinterpret an
        // unknown future enum variant as an executed success.
        return serde_json::from_value(value.clone()).map_err(|_| ());
    }

    // Match ToolCallRecord::effective_disposition for journals written before
    // the explicit field was added. These checks intentionally use only the
    // bounded producer-authored metadata retained by journal_tool_calls.
    if (call.ok != Some(true)
        && metadata
            .get("result_class")
            .and_then(serde_json::Value::as_str)
            == Some(astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS))
        || metadata
            .get("skill_locked_out")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return Ok(astra_services::session_journal::ToolCallDisposition::Rejected);
    }
    if metadata
        .get("surgically_removed")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        || metadata
            .get("skill_reentry_count")
            .is_some_and(|value| !value.is_null())
    {
        return Ok(astra_services::session_journal::ToolCallDisposition::Suppressed);
    }
    if metadata
        .get("result_class")
        .and_then(serde_json::Value::as_str)
        == Some(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS)
    {
        return Ok(astra_services::session_journal::ToolCallDisposition::Reused);
    }
    Ok(astra_services::session_journal::ToolCallDisposition::Executed)
}

fn raw_llm_response_cache_hit_ratio(event: &crate::session_capture::JournalEvent) -> Option<f64> {
    let usage = event
        .raw
        .get("metadata")
        .and_then(|meta| meta.get("response"))
        .and_then(|response| response.get("response"))
        .and_then(|response| response.get("usage"))?;

    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cached_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = usage
        .get("cache_creation_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_input = astra_turn_types::NormalizedPromptCacheUsage::new(
        input_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    )
    .total_input_tokens();
    if total_input == 0 {
        return None;
    }
    Some(cache_read_tokens as f64 / total_input as f64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawPromptFingerprint {
    provider: String,
    model: String,
    system_prompt: String,
    tools_json: String,
}

#[derive(Debug, Clone)]
struct RawPromptTurn {
    turn: u32,
    fingerprint: RawPromptFingerprint,
    cache_hit_ratio: f64,
}

fn detect_raw_prompt_cache_breaks(capture: &SessionCapture) -> Vec<PipelineAlertEntry> {
    let mut turns = Vec::new();
    let mut pending_request: Option<RawPromptFingerprint> = None;
    let mut response_turn = 0u32;

    for event in &capture.events {
        match event.event_type.as_str() {
            "llm_request_full" => {
                pending_request = raw_prompt_fingerprint(event);
            }
            "llm_response_full" => {
                response_turn = response_turn.saturating_add(1);
                let Some(fingerprint) = pending_request.take() else {
                    continue;
                };
                let Some(cache_hit_ratio) = raw_llm_response_cache_hit_ratio(event) else {
                    continue;
                };
                turns.push(RawPromptTurn {
                    turn: response_turn,
                    fingerprint,
                    cache_hit_ratio,
                });
            }
            _ => {}
        }
    }

    let mut alerts = Vec::new();
    for pair in turns.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        let fingerprint_changed = previous.fingerprint != current.fingerprint;
        let crossed_below_floor = previous.cache_hit_ratio >= RAW_CACHE_BREAK_MIN_RATIO
            && current.cache_hit_ratio < RAW_CACHE_BREAK_MIN_RATIO;
        if (fingerprint_changed || crossed_below_floor)
            && current.cache_hit_ratio < RAW_CACHE_BREAK_MIN_RATIO
        {
            alerts.push(PipelineAlertEntry {
                turn: current.turn,
                rule: "prompt_cache_break".into(),
                severity: "warning".into(),
            });
        }
    }
    alerts
}

fn raw_prompt_fingerprint(
    event: &crate::session_capture::JournalEvent,
) -> Option<RawPromptFingerprint> {
    let metadata = event.raw.get("metadata")?;
    let request = metadata.get("request")?;
    let messages = request.get("messages")?.as_array()?;
    let system_prompt = messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .or_else(|| messages.first())
        .map(message_content_text)
        .unwrap_or_default();
    let tools_json =
        serde_json::to_string(request.get("tools").unwrap_or(&Value::Array(vec![]))).ok()?;
    Some(RawPromptFingerprint {
        provider: metadata
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        model: metadata
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        system_prompt,
        tools_json,
    })
}

fn message_content_text(message: &Value) -> String {
    content_value_text(message.get("content").unwrap_or(&Value::Null))
}

fn content_value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| item.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default()),
        _ => value.to_string(),
    }
}

/// Render a human-readable pipeline health summary.
pub fn render_pipeline_health(report: &PipelineHealthReport) -> String {
    let mut out = String::new();
    out.push_str("── Pipeline Health ──\n");

    if report.turns_with_feedback == 0 {
        if report.invalid_events > 0 {
            out.push_str(&format!(
                "  ⚠ Invalid pipeline event payloads: {} (evidence incomplete)\n",
                report.invalid_events
            ));
        }
        out.push_str("  No pipeline feedback events found.\n");
        render_execution_summary(report, &mut out);
        return out;
    }

    out.push_str(&format!(
        "  Turns with feedback: {}\n",
        report.turns_with_feedback
    ));
    out.push_str(&format!(
        "  Avg billable cache-read share: {:.1}%\n",
        report.avg_cache_hit_ratio * 100.0
    ));
    if let Some(coverage) = report.stable_prefix_cache_coverage {
        out.push_str(&format!(
            "  Stable-prefix cache coverage: {:.1}% ({}/{} tokens, {} provider-prefix-v1 observations)\n",
            coverage * 100.0,
            report.stable_prefix_cache_read_tokens,
            report.stable_prefix_cache_eligible_tokens,
            report.provider_prefix_cache_observations,
        ));
    }
    if report.invalid_events > 0 {
        out.push_str(&format!(
            "  ⚠ Invalid pipeline event payloads: {} (evidence incomplete)\n",
            report.invalid_events
        ));
    }

    if !report.cache_hit_ratios.is_empty() {
        let first = report.cache_hit_ratios.first().unwrap_or(&0.0);
        let last = report.cache_hit_ratios.last().unwrap_or(&0.0);
        let trend = if last > first {
            "↑"
        } else if last < first {
            "↓"
        } else {
            "→"
        };
        out.push_str(&format!(
            "  Cache trend: {:.0}% → {:.0}% {}\n",
            first * 100.0,
            last * 100.0,
            trend
        ));
    }

    if report.compaction_count > 0 {
        out.push_str(&format!(
            "  Compactions: {} ({} tokens freed)\n",
            report.compaction_count, report.total_tokens_freed
        ));
    }

    if report.cascade_detected {
        out.push_str("  ⚠ Compaction cascade detected\n");
    }
    if report.prompt_cache_breaks > 0 {
        out.push_str(&format!(
            "  ⚠ Prompt cache breaks: {}\n",
            report.prompt_cache_breaks
        ));
    }

    if !report.alerts.is_empty() {
        out.push_str(&format!("  Alerts: {}\n", report.alerts.len()));
        for alert in &report.alerts {
            out.push_str(&format!(
                "    T{}: [{}] {}\n",
                alert.turn, alert.severity, alert.rule
            ));
        }
    }

    render_execution_summary(report, &mut out);

    out
}

fn render_execution_summary(report: &PipelineHealthReport, out: &mut String) {
    let execution = &report.execution;
    if execution.total_tool_calls == 0 && execution.evidence_complete {
        return;
    }
    let scope = match execution.scope {
        ExecutionTraceScope::Session => "session",
        ExecutionTraceScope::CaseAttempts => "case_attempts",
    };
    out.push_str(&format!(
        "  Execution scope: {scope} captures={}/{}\n",
        execution.captured_capture_count, execution.expected_capture_count,
    ));
    if !execution.evidence_complete {
        out.push_str(&format!(
            "  Execution evidence: incomplete (counts are lower bounds; skipped_lines={} dropped_lines={} integrity_errors={})\n",
            execution.skipped_lines,
            execution.dropped_lines,
            execution.integrity_errors,
        ));
    }
    out.push_str(&format!(
        "  Execution: tools={} executed={} success={} failed={} rejected={} reused={} suppressed={} deferred={} unknown={} unknown_disposition={}\n",
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
        out.push_str(&format!(
            "  Work settlements: attempts={} success={} rejected={}\n",
            execution.settlement_attempts,
            execution.successful_settlements,
            execution.rejected_settlements,
        ));
    }
    for (reason, count) in &execution.runtime_rejection_reasons {
        out.push_str(&format!("  Runtime rejections: {} × {}\n", count, reason));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_capture::JournalEvent;

    fn make_feedback_event(turn: u32, cache_hit_ratio: f64) -> JournalEvent {
        let cache_read = (cache_hit_ratio * 1_000.0).round() as u64;
        let fresh = 1_000u64.saturating_sub(cache_read);
        let prompt_cache_identity =
            astra_turn_types::PromptCacheIdentityV1::from_prefixes(&[], &[], "provider-prefix-v1")
                .expect("valid provider-prefix identity");
        JournalEvent {
            event_type: "pipeline_feedback".into(),
            raw: serde_json::json!({
                "type": "pipeline_feedback",
                "turn": turn,
                "metadata": {
                    "kind": "Feedback",
                    "turn": turn,
                    "runtime_feedback": {
                        "schema_version": astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
                        "identity": {
                            "session_id": "session-1",
                            "run_id": format!("run-{turn}"),
                            "agent_id": "agent-1",
                            "model_id": "deepseek-v4-flash",
                            "topology": "server_only"
                        },
                        "progress": {
                            "session_turn": turn,
                            "agentic_round_index": 0,
                            "llm_rounds_completed": 1,
                            "slice_round_limit": 10,
                            "slice_rounds_remaining": 9
                        },
                        "context": {
                            "prompt_cache_identity": prompt_cache_identity,
                            "token_pressure": 0.1,
                            "compaction_tier": "normal"
                        },
                        "request_usage": {
                            "prompt": fresh,
                            "cache_read": cache_read,
                            "cache_creation": 0,
                            "completion": 300
                        },
                        "run_usage": {
                            "prompt": fresh,
                            "cache_read": cache_read,
                            "cache_creation": 0,
                            "completion": 300
                        },
                        "policy_feedback": { "state": "not_evaluated" },
                        "was_truncated": false
                    }
                }
            }),
        }
    }

    fn make_tool_event(tool_calls: serde_json::Value) -> JournalEvent {
        JournalEvent {
            event_type: "turn".into(),
            raw: serde_json::json!({
                "turn": 1,
                "tool_calls": tool_calls,
            }),
        }
    }

    fn make_compaction_event(turn: u32, tokens_freed: u64) -> JournalEvent {
        JournalEvent {
            event_type: "pipeline_compaction_audit".into(),
            raw: serde_json::json!({
                "type": "pipeline_compaction_audit",
                "turn": turn,
                "metadata": {
                    "kind": "CompactionAudit",
                    "turn": turn,
                    "compaction_strategy": "tool_result_clearing",
                    "tokens_freed": tokens_freed,
                }
            }),
        }
    }

    fn make_step_compaction_event(turn: u32, tokens_saved: u64) -> JournalEvent {
        JournalEvent {
            event_type: "CompactionFired".into(),
            raw: serde_json::json!({
                "event_type": "CompactionFired",
                "payload": {
                    "kind": "resume",
                    "tokens_saved": tokens_saved,
                    "trace_context": { "visible_turn": turn },
                }
            }),
        }
    }

    fn make_alert_event(turn: u32, rule: &str, severity: &str) -> JournalEvent {
        JournalEvent {
            event_type: "pipeline_alert".into(),
            raw: serde_json::json!({
                "type": "pipeline_alert",
                "turn": turn,
                "metadata": {
                    "kind": "Alert",
                    "turn": turn,
                    "alert_rule": rule,
                    "alert_severity": severity,
                }
            }),
        }
    }

    fn make_capture(events: Vec<JournalEvent>) -> SessionCapture {
        SessionCapture {
            session_id: "test-session".into(),
            journal_path: std::path::PathBuf::from("/tmp/test.jsonl"),
            events,
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        }
    }

    fn make_llm_request_event(
        model: &str,
        provider: &str,
        system_prompt: Value,
        tools: Value,
    ) -> JournalEvent {
        JournalEvent {
            event_type: "llm_request_full".into(),
            raw: serde_json::json!({
                "type": "llm_request_full",
                "metadata": {
                    "model": model,
                    "provider": provider,
                    "request": {
                        "messages": [
                            {
                                "role": "system",
                                "content": system_prompt,
                            },
                            {
                                "role": "user",
                                "content": "Reply ACK.",
                            }
                        ],
                        "tools": tools,
                    }
                }
            }),
        }
    }

    fn make_llm_response_event(
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_tokens: u64,
    ) -> JournalEvent {
        JournalEvent {
            event_type: "llm_response_full".into(),
            raw: serde_json::json!({
                "type": "llm_response_full",
                "metadata": {
                    "response": {
                        "response": {
                            "usage": {
                                "input_tokens": input_tokens,
                                "cached_input_tokens": cached_input_tokens,
                                "cache_creation_tokens": cache_creation_tokens,
                            }
                        }
                    }
                }
            }),
        }
    }

    #[test]
    fn empty_session_produces_empty_report() {
        let capture = make_capture(vec![]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 0);
        assert_eq!(report.avg_cache_hit_ratio, 0.0);
        assert_eq!(report.execution, ExecutionTraceReport::default());
    }

    #[test]
    fn execution_trace_separates_rejected_settlement_from_successful_transitions() {
        let capture = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "settle_work_item",
                "call_id": "settle-1",
                "ok": true,
                "disposition": "executed"
            },
            {
                "name": "settle_work_item",
                "call_id": "settle-2",
                "ok": false,
                "disposition": "rejected",
                "error_kind": "contract_violation",
                "result_full": "{\"error_kind\":\"work_settlement_evidence_required\"}"
            },
            {
                "name": "bash",
                "call_id": "bash-1",
                "ok": true,
                "disposition": "executed"
            },
            {
                "name": "settle_work_item",
                "call_id": "settle-3",
                "ok": true,
                "disposition": "executed"
            }
        ]))]);

        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.execution.total_tool_calls, 4);
        assert_eq!(report.execution.executed_tool_calls, 3);
        assert_eq!(report.execution.successful_tool_calls, 3);
        assert_eq!(report.execution.failed_tool_calls, 0);
        assert_eq!(report.execution.rejected_tool_calls, 1);
        assert_eq!(report.execution.settlement_attempts, 3);
        assert_eq!(report.execution.successful_settlements, 2);
        assert_eq!(report.execution.rejected_settlements, 1);
        assert_eq!(
            report
                .execution
                .runtime_rejection_reasons
                .get("work_settlement_evidence_required"),
            Some(&1)
        );

        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("Work settlements: attempts=3 success=2 rejected=1"));
        assert!(rendered.contains("Runtime rejections: 1 × work_settlement_evidence_required"));
    }

    #[test]
    fn execution_trace_does_not_count_suppressed_placeholders_as_execution() {
        let capture = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "bash-success",
                "ok": true,
                "disposition": "executed"
            },
            {
                "name": "bash",
                "call_id": "bash-failure",
                "ok": false,
                "disposition": "executed"
            },
            {
                "name": "settle_work_item",
                "call_id": "settle-rejected",
                "ok": false,
                "disposition": "rejected",
                "error_kind": "contract_violation"
            },
            {
                "name": "settle_work_item",
                "call_id": "settle-success",
                "ok": true,
                "disposition": "executed"
            },
            {
                "name": "(surgically_removed)",
                "call_id": "suppressed-1",
                "ok": true,
                "disposition": "suppressed",
                "surgically_removed": true,
                "original_tool_name": "bash"
            },
            {
                "name": "settle_work_item",
                "call_id": "settle-suppressed",
                "ok": true,
                "disposition": "suppressed"
            }
        ]))]);

        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.execution.total_tool_calls, 6);
        assert_eq!(report.execution.executed_tool_calls, 3);
        assert_eq!(report.execution.successful_tool_calls, 2);
        assert_eq!(report.execution.failed_tool_calls, 1);
        assert_eq!(report.execution.rejected_tool_calls, 1);
        assert_eq!(report.execution.suppressed_tool_calls, 2);
        assert_eq!(report.execution.settlement_attempts, 3);
        assert_eq!(report.execution.successful_settlements, 1);
        assert_eq!(report.execution.rejected_settlements, 1);

        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains(
            "Execution: tools=6 executed=3 success=2 failed=1 rejected=1 reused=0 suppressed=2 deferred=0 unknown=0 unknown_disposition=0"
        ));
    }

    #[test]
    fn execution_trace_reuses_legacy_disposition_fallbacks_and_rejects_unknown_values() {
        let capture = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "(surgically_removed)",
                "call_id": "legacy-suppressed",
                "ok": true,
                "surgically_removed": true
            },
            {
                "name": "skill",
                "call_id": "legacy-reentry",
                "ok": true,
                "skill_reentry_count": 1
            },
            {
                "name": "skill",
                "call_id": "legacy-locked",
                "ok": false,
                "skill_locked_out": true
            },
            {
                "name": "read_file",
                "call_id": "legacy-reused",
                "ok": true,
                "result_class": "noop_or_cached"
            },
            {
                "name": "bash",
                "call_id": "explicit-deferred",
                "ok": true,
                "disposition": "deferred"
            },
            {
                "name": "bash",
                "call_id": "explicit-unknown",
                "ok": true,
                "disposition": "future_disposition"
            }
        ]))]);

        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.execution.total_tool_calls, 6);
        assert_eq!(report.execution.executed_tool_calls, 0);
        assert_eq!(report.execution.successful_tool_calls, 0);
        assert_eq!(report.execution.failed_tool_calls, 0);
        assert_eq!(report.execution.rejected_tool_calls, 1);
        assert_eq!(report.execution.reused_tool_calls, 1);
        assert_eq!(report.execution.suppressed_tool_calls, 2);
        assert_eq!(report.execution.deferred_tool_calls, 1);
        assert_eq!(report.execution.unknown_disposition_tool_calls, 1);
        assert!(!report.execution.evidence_complete);
    }

    #[test]
    fn execution_trace_aggregates_distinct_root_attempt_captures() {
        let mut first = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "first-failed",
                "ok": false,
                "disposition": "executed"
            }
        ]))]);
        first.session_id = "first-session".into();
        let mut second = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "second-succeeded",
                "ok": true,
                "disposition": "executed"
            }
        ]))]);
        second.session_id = "second-session".into();

        let report = analyze_execution_traces([&first, &second], 2);
        assert_eq!(report.scope, ExecutionTraceScope::CaseAttempts);
        assert_eq!(report.captured_capture_count, 2);
        assert_eq!(report.expected_capture_count, 2);
        assert_eq!(report.total_tool_calls, 2);
        assert_eq!(report.executed_tool_calls, 2);
        assert_eq!(report.successful_tool_calls, 1);
        assert_eq!(report.failed_tool_calls, 1);
        assert!(report.evidence_complete);
    }

    #[test]
    fn execution_trace_marks_missing_root_attempt_capture_incomplete() {
        let mut first = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "first-failed",
                "ok": false,
                "disposition": "executed"
            }
        ]))]);
        first.session_id = "first-session".into();

        let report = analyze_execution_traces([&first], 2);
        assert_eq!(report.captured_capture_count, 1);
        assert_eq!(report.expected_capture_count, 2);
        assert_eq!(report.total_tool_calls, 1);
        assert!(!report.evidence_complete);
    }

    #[test]
    fn execution_trace_rejects_duplicate_root_session_capture() {
        let mut first = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "same-call",
                "ok": true,
                "disposition": "executed"
            }
        ]))]);
        first.session_id = "same-session".into();
        let mut duplicate = first.clone();
        duplicate.events[0].raw["tool_calls"][0]["call_id"] = "different-call".into();

        let report = analyze_execution_traces([&first, &duplicate], 2);
        assert_eq!(report.captured_capture_count, 2);
        assert_eq!(report.total_tool_calls, 1);
        assert!(!report.evidence_complete);
    }

    #[test]
    fn execution_trace_marks_skipped_rows_as_lower_bound() {
        let mut capture = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "bash-1",
                "ok": true,
                "disposition": "executed"
            }
        ]))]);
        capture.skipped_lines = 2;

        let report = analyze_pipeline_health(&capture);
        assert!(!report.execution.evidence_complete);
        assert_eq!(report.execution.total_tool_calls, 1);
        assert_eq!(report.execution.skipped_lines, 2);
        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("counts are lower bounds"));
        assert!(rendered.contains("skipped_lines=2"));
    }

    #[test]
    fn execution_trace_does_not_turn_integrity_conflict_into_complete_zero() {
        let mut capture = make_capture(vec![make_tool_event(serde_json::json!([
            {
                "name": "bash",
                "call_id": "bash-1",
                "ok": true,
                "disposition": "executed"
            }
        ]))]);
        capture.integrity_errors = 1;

        let report = analyze_pipeline_health(&capture);
        assert!(!report.execution.evidence_complete);
        assert_eq!(report.execution.total_tool_calls, 0);
        assert_eq!(report.execution.integrity_errors, 1);
        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("Execution evidence: incomplete"));
        assert!(rendered.contains("integrity_errors=1"));
    }

    #[test]
    fn feedback_events_produce_cache_trend() {
        let capture = make_capture(vec![
            make_feedback_event(1, 0.0),
            make_feedback_event(2, 0.7),
            make_feedback_event(3, 0.85),
            make_feedback_event(4, 0.9),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 4);
        assert_eq!(report.cache_hit_ratios.len(), 4);
        assert!(report.avg_cache_hit_ratio > 0.5);
    }

    #[test]
    fn negative_feedback_pressure_marks_health_evidence_incomplete() {
        let mut event = make_feedback_event(1, 0.5);
        event.raw["metadata"]["runtime_feedback"]["context"]["token_pressure"] =
            serde_json::json!(-0.1);
        let capture = make_capture(vec![event]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 0);
        assert_eq!(report.invalid_events, 1);
    }

    #[test]
    fn feedback_without_required_policy_projection_is_incomplete() {
        let mut event = make_feedback_event(1, 0.5);
        event.raw["metadata"]["runtime_feedback"]
            .as_object_mut()
            .expect("runtime feedback object")
            .remove("policy_feedback");
        let report = analyze_pipeline_health(&make_capture(vec![event]));
        assert_eq!(report.turns_with_feedback, 0);
        assert_eq!(report.invalid_events, 1);
    }

    #[test]
    fn llm_response_usage_fallback_produces_cache_trend() {
        let capture = make_capture(vec![
            make_llm_response_event(9_984, 5, 0),
            make_llm_response_event(162, 10_112, 0),
            make_llm_response_event(172, 10_112, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 3);
        assert_eq!(report.cache_hit_ratios.len(), 3);
        assert!(report.cache_hit_ratios[0] < 0.01);
        assert!(report.cache_hit_ratios[1] > 0.9);
        assert!(report.avg_cache_hit_ratio > 0.6);
    }

    #[test]
    fn stable_prefix_coverage_is_reported_separately_from_billable_share() {
        let mut first = make_feedback_event(1, 0.5);
        first.raw["metadata"]["runtime_feedback"]["context"]["estimated_cache_eligible_tokens"] =
            serde_json::json!(800);
        let mut second = make_feedback_event(2, 0.9);
        second.raw["metadata"]["runtime_feedback"]["context"]["estimated_cache_eligible_tokens"] =
            serde_json::json!(800);

        let report = analyze_pipeline_health(&make_capture(vec![first, second]));

        assert_eq!(report.stable_prefix_cache_eligible_tokens, 1_600);
        assert_eq!(report.stable_prefix_cache_read_tokens, 1_300);
        assert_eq!(report.stable_prefix_cache_observations, 2);
        assert_eq!(report.provider_prefix_cache_observations, 2);
        assert_eq!(report.stable_prefix_cache_coverage, Some(0.8125));
        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("Avg billable cache-read share"));
        assert!(rendered.contains("Stable-prefix cache coverage: 81.2%"));
        assert!(rendered.contains("2 provider-prefix-v1 observations"));
    }

    #[test]
    fn stable_prefix_coverage_does_not_claim_non_prefix_layouts() {
        let mut event = make_feedback_event(1, 0.9);
        event.raw["metadata"]["runtime_feedback"]["context"]["estimated_cache_eligible_tokens"] =
            serde_json::json!(800);
        event.raw["metadata"]["runtime_feedback"]["context"]["prompt_cache_identity"] =
            serde_json::to_value(
                astra_turn_types::PromptCacheIdentityV1::from_prefixes(
                    &[],
                    &[],
                    "explicit-breakpoints-v1",
                )
                .expect("valid explicit-breakpoint identity"),
            )
            .expect("identity serializes");

        let report = analyze_pipeline_health(&make_capture(vec![event]));

        assert_eq!(report.stable_prefix_cache_observations, 1);
        assert_eq!(report.provider_prefix_cache_observations, 0);
        assert_eq!(report.stable_prefix_cache_coverage, None);
    }

    #[test]
    fn pipeline_feedback_takes_precedence_over_raw_usage_fallback() {
        let capture = make_capture(vec![
            make_llm_response_event(100, 9_900, 0),
            make_feedback_event(1, 0.2),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 1);
        assert_eq!(report.cache_hit_ratios, vec![0.2]);
    }

    #[test]
    fn raw_prompt_break_detection_flags_cache_drop_without_pipeline_alerts() {
        let capture = make_capture(vec![
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(1_000, 0, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.prompt_cache_breaks, 1);
        assert_eq!(
            report
                .alerts
                .iter()
                .filter(|alert| alert.rule == "prompt_cache_break")
                .count(),
            1
        );
    }

    #[test]
    fn explicit_prompt_cache_break_alerts_suppress_raw_duplicates() {
        let capture = make_capture(vec![
            make_alert_event(3, "prompt_cache_break", "warning"),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(1_000, 0, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.prompt_cache_breaks, 1);
        assert_eq!(
            report
                .alerts
                .iter()
                .filter(|alert| alert.rule == "prompt_cache_break")
                .count(),
            1
        );
    }

    #[test]
    fn compaction_events_accumulate() {
        let capture = make_capture(vec![
            make_compaction_event(3, 2000),
            make_step_compaction_event(5, 3000),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.compaction_count, 2);
        assert_eq!(report.total_tokens_freed, 5000);
    }

    #[test]
    fn cascade_alert_detected() {
        let capture = make_capture(vec![make_alert_event(7, "compaction_cascade", "Warning")]);
        let report = analyze_pipeline_health(&capture);
        assert!(report.cascade_detected);
        assert_eq!(report.alerts.len(), 1);
    }

    #[test]
    fn render_produces_readable_output() {
        let capture = make_capture(vec![
            make_feedback_event(1, 0.0),
            make_feedback_event(2, 0.8),
            make_feedback_event(3, 0.9),
            make_compaction_event(2, 1500),
        ]);
        let report = analyze_pipeline_health(&capture);
        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("Avg billable cache-read share"));
        assert!(rendered.contains("Compactions: 1"));
        assert!(rendered.contains("Cache trend"));
    }
}
