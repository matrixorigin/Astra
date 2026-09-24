//! Error/limit enrichment and TurnGuard quality for the headless tool round (CLI §5.5).

use std::time::{Duration, Instant};

use crate::guardrails::error_recovery::{ErrorCategory, build_recovery_message_with_evidence};
use crate::guardrails::turn_guard::TurnGuard;
use crate::headless_tool_assembly::{HeadlessRoundToolIdx, headless_timeout_aborted_tool_names};
use crate::result_quality::ResultQuality;
use astra_pipeline::step_checkpoint;
use astra_pipeline::step_protocol::{
    CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache, StepCheckpoint, epoch_ms,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_text_utils::semantic_dedup::SemanticDedup;

use serde_json::Value;

/// UI hooks for headless postprocess (CLI maps to colored `eprintln!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessOutputEnrichSignal {
    ResourceLimitObserved { tool: String },
}

/// Whether the tool implementation ran for a returned terminal result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessExecutionDisposition {
    Executed,
    RejectedBeforeExecution,
}

/// Mutable state used while enriching one headless tool result.
pub struct HeadlessOutputEnrichCtx<'a> {
    pub turn_guard: &'a mut TurnGuard,
    /// Runtime-authored guidance, separate from the executor's result document.
    pub advisories: &'a mut Vec<String>,
}

/// Immutable executor output and mutable classification for one tool result.
pub struct HeadlessOutputEnrichRequest<'a> {
    pub name: &'a str,
    pub result_str: &'a str,
    pub is_err: &'a mut bool,
    pub source_error_kind: Option<ErrorCategory>,
    pub source_recovery_evidence: Option<&'a astra_core::ToolFailureEvidence>,
    pub tool_already_restricted: bool,
    pub execution_disposition: HeadlessExecutionDisposition,
}

/// `true` when resource-limit handling forced error-quality treatment (matches CLI `resource_limit_recorded`).
pub fn enrich_headless_tool_output_for_errors_and_limits(
    request: HeadlessOutputEnrichRequest<'_>,
    ctx: &mut HeadlessOutputEnrichCtx<'_>,
    mut on_signal: impl FnMut(HeadlessOutputEnrichSignal),
) -> bool {
    let HeadlessOutputEnrichRequest {
        name,
        result_str,
        is_err,
        source_error_kind,
        source_recovery_evidence,
        tool_already_restricted,
        execution_disposition,
    } = request;
    let mut resource_limit_recorded = false;
    if *is_err && !tool_already_restricted {
        let category = source_error_kind.unwrap_or(ErrorCategory::Unknown);

        if matches!(category, ErrorCategory::ResourceLimit) {
            if execution_disposition == HeadlessExecutionDisposition::Executed {
                ctx.turn_guard.health.record_resource_limit_failure(name);
            }
            ctx.turn_guard.errors.record_error(category);
            resource_limit_recorded = true;
            on_signal(HeadlessOutputEnrichSignal::ResourceLimitObserved {
                tool: name.to_string(),
            });
        }

        if category.is_retryable() {
            ctx.turn_guard.errors.record_retry(false);
        }

        let avoidance_advised = ctx.turn_guard.health.health_avoidance_tools();
        let recovery_msg = build_recovery_message_with_evidence(
            name,
            result_str,
            category,
            &avoidance_advised,
            source_recovery_evidence,
        );
        ctx.advisories.push(recovery_msg);
    }

    resource_limit_recorded
}

/// Record result quality without rewriting the executor's result document.
pub struct HeadlessResultQualityRequest<'a> {
    pub name: &'a str,
    pub result_str: &'a str,
    pub source_error_kind: Option<ErrorCategory>,
    pub execution_failed: bool,
    pub execution_disposition: HeadlessExecutionDisposition,
    pub resource_limit_recorded: bool,
}

pub fn append_headless_result_quality_feedback(
    request: HeadlessResultQualityRequest<'_>,
    turn_guard: &mut TurnGuard,
    advisories: &mut Vec<String>,
) -> ResultQuality {
    let HeadlessResultQualityRequest {
        name,
        result_str,
        source_error_kind,
        execution_failed,
        execution_disposition,
        resource_limit_recorded,
    } = request;
    let result_quality = if resource_limit_recorded {
        ResultQuality::Error
    } else if execution_disposition == HeadlessExecutionDisposition::RejectedBeforeExecution {
        turn_guard.record_rejected_tool_result_with_kind(source_error_kind)
    } else if execution_failed {
        turn_guard.record_failed_tool_result_with_kind(name, source_error_kind)
    } else {
        turn_guard.record_successful_tool_result_with_kind(name, result_str, source_error_kind)
    };
    // Execution errors already received classified recovery evidence in
    // `enrich_headless_tool_output_for_errors_and_limits`. Appending generic
    // quality feedback here creates a second, often contradictory instruction
    // (for example "try another tool" after a route-scoped transport failure).
    if !execution_failed
        && !resource_limit_recorded
        && let Some(feedback) = turn_guard.result_feedback(name, result_quality)
    {
        advisories.push(feedback);
    }
    result_quality
}

/// Step scheduling wall-clock budget for the headless tool round (one `StepRecorder` act).
#[derive(Debug, Clone)]
pub struct HeadlessStepDeadline {
    start: Instant,
    timeout_ms: u64,
}

impl HeadlessStepDeadline {
    #[must_use]
    pub fn from_scheduling_timeout_ms(timeout_ms: u64) -> Self {
        Self {
            start: Instant::now(),
            timeout_ms,
        }
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    #[must_use]
    pub fn is_past_deadline(&self) -> bool {
        self.elapsed_ms() > self.timeout_ms
    }

    /// When past deadline: how many tools were not yet written to `tool_results`, and their names.
    #[must_use]
    pub fn step_timeout_abort(
        &self,
        indices: &[HeadlessRoundToolIdx],
        completed_tool_results_len: usize,
        server_tool_calls: &[Value],
        synthetic_tool_name: impl FnMut(usize) -> String,
    ) -> Option<(usize, Vec<String>)> {
        if !self.is_past_deadline() {
            return None;
        }
        let aborted_count = indices.len().saturating_sub(completed_tool_results_len);
        let aborted_tools = headless_timeout_aborted_tool_names(
            indices,
            completed_tool_results_len,
            server_tool_calls,
            synthetic_tool_name,
        );
        Some((aborted_count, aborted_tools))
    }
}

/// Mutable handles for [`record_headless_cacheable_success_and_semantic_hint`].
pub struct HeadlessCacheableRecordCtx<'a> {
    /// Raw provider observation used for cache identity and similarity.
    pub observation: &'a str,
    /// Per-invocation guidance, kept separate from the provider observation.
    pub advisories: &'a mut Vec<String>,
    pub call_id: Option<&'a str>,
    pub turn_index: usize,
    pub semantic_context_generation: u64,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub step_recorder: &'a mut StepRecorder,
    pub semantic_dedup: &'a mut SemanticDedup,
}

/// After a successful read, attach an invocation replay result when its full
/// identity is available and record the semantic duplicate hint. A read with
/// no replay identity still contributes the hint without becoming reusable.
pub fn record_headless_cacheable_success_and_semantic_hint_if_ok(
    name: &str,
    args: &Value,
    idem_key: Option<&IdempotencyKey>,
    ctx: HeadlessCacheableRecordCtx<'_>,
    is_err: bool,
) {
    if is_err {
        return;
    }
    record_headless_cacheable_success_and_semantic_hint(name, args, idem_key, ctx);
}

pub fn record_headless_cacheable_success_and_semantic_hint(
    name: &str,
    args: &Value,
    idem_key: Option<&IdempotencyKey>,
    ctx: HeadlessCacheableRecordCtx<'_>,
) {
    if let Some(idem_key) = idem_key {
        let cached_result = CachedToolResult {
            tool_name: name.to_string(),
            output: ctx.observation.to_string(),
            is_error: false,
            cached_at: epoch_ms(),
            context_signature: idem_key.context_signature.clone(),
        };
        if let Some(call_id) = ctx.call_id {
            ctx.step_recorder
                .attach_cached_result_for_call(call_id, cached_result.clone());
        } else {
            ctx.step_recorder
                .attach_cached_result(cached_result.clone());
        }
        ctx.idempotency_cache.record(idem_key, cached_result);
    }
    if let Some(hint) = ctx
        .semantic_dedup
        .near_duplicate_hint_for_observation_with_generation(
            ctx.observation,
            name,
            args,
            ctx.turn_index,
            ctx.semantic_context_generation,
        )
    {
        ctx.advisories.push(hint);
    }
}

/// Best-effort light checkpoint after each tool (matches CLI headless path).
pub fn try_write_light_headless_step_checkpoint(
    user_id: &str,
    session_id: &str,
    step_recorder: &StepRecorder,
) {
    if let Some(light) = step_recorder.build_light_checkpoint() {
        let cp = StepCheckpoint::Light(light);
        let n = step_recorder.summary().checkpoints;
        let _ = step_checkpoint::write_step_checkpoint(user_id, session_id, n, &cp);
    }
}

#[must_use]
pub fn format_headless_tool_duration(elapsed: Duration) -> String {
    if elapsed.as_secs_f64() >= 1.0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_duration_seconds_vs_ms() {
        assert_eq!(
            format_headless_tool_duration(Duration::from_millis(1500)),
            "1.5s"
        );
        assert_eq!(
            format_headless_tool_duration(Duration::from_millis(40)),
            "40ms"
        );
    }

    #[test]
    fn enrich_resource_limit_classifies_without_hard_restricting_tool() {
        let mut tg = TurnGuard::new();
        let mut out = "out of memory".to_string();
        let mut is_err = true;
        let mut signals = Vec::new();
        let mut ctx = HeadlessOutputEnrichCtx {
            turn_guard: &mut tg,
            advisories: &mut Vec::new(),
        };
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "bash",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: Some(astra_core::ErrorKind::ResourceLimit),
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut ctx,
            |s| signals.push(s),
        );
        assert!(rec);
        assert_eq!(
            signals,
            vec![HeadlessOutputEnrichSignal::ResourceLimitObserved {
                tool: "bash".into()
            }]
        );
        assert!(out.contains("out of memory"));
    }

    #[test]
    fn enrich_resource_limit_does_not_hard_restrict_read_only_tools() {
        let mut tg = TurnGuard::new();
        let mut out = "read failed: Resource temporarily unavailable".to_string();
        let mut is_err = true;
        let mut signals = Vec::new();
        let mut ctx = HeadlessOutputEnrichCtx {
            turn_guard: &mut tg,
            advisories: &mut Vec::new(),
        };

        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "read_file",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: Some(astra_core::ErrorKind::ResourceLimit),
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut ctx,
            |s| signals.push(s),
        );

        assert!(rec);
        assert_eq!(
            signals,
            vec![HeadlessOutputEnrichSignal::ResourceLimitObserved {
                tool: "read_file".into()
            }]
        );
    }

    #[test]
    fn rejected_resource_failure_keeps_error_pressure_without_execution_health() {
        let mut tg = TurnGuard::new();
        let mut is_err = true;
        let mut advisories = Vec::new();
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "read_file",
                result_str: "capacity unavailable before execution",
                is_err: &mut is_err,
                source_error_kind: Some(astra_core::ErrorKind::ResourceLimit),
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::RejectedBeforeExecution,
            },
            &mut HeadlessOutputEnrichCtx {
                turn_guard: &mut tg,
                advisories: &mut advisories,
            },
            |_| {},
        );
        let quality = append_headless_result_quality_feedback(
            HeadlessResultQualityRequest {
                name: "read_file",
                result_str: "capacity unavailable before execution",
                source_error_kind: Some(astra_core::ErrorKind::ResourceLimit),
                execution_failed: true,
                execution_disposition: HeadlessExecutionDisposition::RejectedBeforeExecution,
                resource_limit_recorded,
            },
            &mut tg,
            &mut advisories,
        );

        assert_eq!(quality, ResultQuality::Error);
        assert_eq!(tg.errors.total_errors, 1);
        assert!(tg.health.get("read_file").is_none());
        assert!(
            !advisories.is_empty(),
            "recovery guidance must be preserved"
        );
    }

    #[test]
    fn successful_output_text_cannot_create_a_resource_failure() {
        let mut tg = TurnGuard::new();
        let mut out = "fork: retry: Resource temporarily unavailable".to_string();
        let mut is_err = false;
        let mut signals = Vec::new();
        let mut ctx = HeadlessOutputEnrichCtx {
            turn_guard: &mut tg,
            advisories: &mut Vec::new(),
        };
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "bash",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut ctx,
            |s| signals.push(s),
        );
        assert!(!rec);
        assert!(!is_err);
        assert!(signals.is_empty());
    }

    #[test]
    fn recovery_enrichment_preserves_structured_result_bytes() {
        // Runtime guidance must not change the tool's document shape or make
        // an otherwise complete JSON result impossible to recover from a journal.
        for original in [
            r#"{ "status": "failed", "executed": false, "error_kind": "probe_rejected" }"#,
            r#"["opaque", {"failure": true}]"#,
            r#""opaque failure""#,
            "null",
        ] {
            let mut turn_guard = TurnGuard::new();
            let mut output = original.to_string();
            let mut is_error = true;
            let mut advisories = Vec::new();
            enrich_headless_tool_output_for_errors_and_limits(
                HeadlessOutputEnrichRequest {
                    name: "probe",
                    result_str: &mut output,
                    is_err: &mut is_error,
                    source_error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
                    source_recovery_evidence: None,
                    tool_already_restricted: false,
                    execution_disposition: HeadlessExecutionDisposition::Executed,
                },
                &mut HeadlessOutputEnrichCtx {
                    turn_guard: &mut turn_guard,
                    advisories: &mut advisories,
                },
                |_| {},
            );
            assert_eq!(output, original, "runtime guidance changed tool evidence");
            assert!(
                !advisories.is_empty(),
                "opaque JSON errors still need recovery guidance"
            );
        }
    }

    #[test]
    fn successful_waiting_receipt_is_not_reinterpreted_as_failure() {
        let mut turn_guard = TurnGuard::new();
        let mut output = json!({
            "status": "waiting",
            "agent_id": "reviewer-1",
            "reason": "executor_offline"
        })
        .to_string();
        let expected = output.clone();
        let mut is_error = false;
        let mut context = HeadlessOutputEnrichCtx {
            turn_guard: &mut turn_guard,
            advisories: &mut Vec::new(),
        };

        let resource_limit = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "agent",
                result_str: &mut output,
                is_err: &mut is_error,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut context,
            |_| panic!("a typed success is not a resource failure"),
        );

        assert!(!resource_limit);
        assert!(!is_error);
        assert_eq!(output, expected);
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["status"],
            "waiting"
        );
    }

    #[test]
    fn failure_metadata_is_not_hidden_by_waiting_text() {
        let mut turn_guard = TurnGuard::new();
        let mut output = json!({"status": "waiting"}).to_string();
        let mut is_error = true;
        let mut advisories = Vec::new();

        enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "agent",
                result_str: &mut output,
                is_err: &mut is_error,
                source_error_kind: Some(astra_core::ErrorKind::ToolUnavailable),
                source_recovery_evidence: None,
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut HeadlessOutputEnrichCtx {
                turn_guard: &mut turn_guard,
                advisories: &mut advisories,
            },
            |_| panic!("ToolUnavailable is not ResourceLimit"),
        );

        assert!(is_error);
        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains("not available"));
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({"status": "waiting"})
        );
    }

    #[test]
    fn source_authored_large_input_evidence_drives_targeted_recovery() {
        let mut turn_guard = TurnGuard::new();
        let mut output = "opaque external failure".to_string();
        let mut is_error = true;
        let mut context = HeadlessOutputEnrichCtx {
            turn_guard: &mut turn_guard,
            advisories: &mut Vec::new(),
        };
        let evidence = astra_core::ToolFailureEvidence::new(
            astra_core::ErrorKind::ToolInvalidArgs,
            astra_core::ToolFailureCause::InputTooLarge,
            false,
            vec![astra_core::ToolRecoveryAction::ReadTargetedRange],
        );

        enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "read_file",
                result_str: &mut output,
                is_err: &mut is_error,
                source_error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
                source_recovery_evidence: Some(&evidence),
                tool_already_restricted: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
            },
            &mut context,
            |_| {},
        );

        let guidance = context.advisories.join("\n");
        assert_eq!(output, "opaque external failure");
        assert!(guidance.contains("targeted line/range read"), "{guidance}");
        assert!(!guidance.contains("retry the same tool"), "{guidance}");
    }

    #[test]
    fn append_feedback_after_success() {
        let mut tg = TurnGuard::new();
        let out = "ok".to_string();
        let _q = append_headless_result_quality_feedback(
            HeadlessResultQualityRequest {
                name: "bash",
                result_str: &out,
                source_error_kind: None,
                execution_failed: false,
                execution_disposition: HeadlessExecutionDisposition::Executed,
                resource_limit_recorded: false,
            },
            &mut tg,
            &mut Vec::new(),
        );
        assert_eq!(out, "ok");
    }

    #[test]
    fn successful_execution_keeps_error_shaped_content_as_content() {
        for out in [
            "Error: this is a line from the inspected file".to_string(),
            json!({"status": "failed", "content": "quoted log record"}).to_string(),
        ] {
            let mut tg = TurnGuard::new();
            let quality = append_headless_result_quality_feedback(
                HeadlessResultQualityRequest {
                    name: "read_file",
                    result_str: &out,
                    source_error_kind: None,
                    execution_failed: false,
                    execution_disposition: HeadlessExecutionDisposition::Executed,
                    resource_limit_recorded: false,
                },
                &mut tg,
                &mut Vec::new(),
            );
            assert_eq!(
                quality,
                ResultQuality::Success,
                "content was reclassified: {out}"
            );
        }
    }

    #[test]
    fn append_feedback_preserves_structured_execution_failure() {
        let mut tg = TurnGuard::new();
        let out = json!({
            "status": "failed",
            "error": "Unknown tool `outline`",
            "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
            "retryable": false
        })
        .to_string();
        let original = out.clone();

        let quality = append_headless_result_quality_feedback(
            HeadlessResultQualityRequest {
                name: "outline",
                result_str: &out,
                source_error_kind: Some(astra_core::ErrorKind::ToolNotFound),
                execution_failed: true,
                execution_disposition: HeadlessExecutionDisposition::Executed,
                resource_limit_recorded: false,
            },
            &mut tg,
            &mut Vec::new(),
        );

        assert_eq!(quality, ResultQuality::Error);
        assert_eq!(out, original);
        let health = tg.health.get("outline").expect("tool health");
        assert_eq!(health.total_failures, 1);
        assert_eq!(health.consecutive_failures, 1);
        assert!(
            !out.contains("Use another tool only"),
            "classified recovery must not receive a second generic error instruction: {out}"
        );
    }

    #[test]
    fn rejected_attempt_records_error_without_execution_health() {
        let mut tg = TurnGuard::new();
        let quality = append_headless_result_quality_feedback(
            HeadlessResultQualityRequest {
                name: "outline",
                result_str: "unknown tool",
                source_error_kind: Some(astra_core::ErrorKind::ToolNotFound),
                execution_failed: true,
                execution_disposition: HeadlessExecutionDisposition::RejectedBeforeExecution,
                resource_limit_recorded: false,
            },
            &mut tg,
            &mut Vec::new(),
        );

        assert_eq!(quality, ResultQuality::Error);
        assert_eq!(tg.errors.total_errors, 1);
        assert!(tg.health.get("outline").is_none());
    }

    #[test]
    fn step_timeout_abort_none_under_long_budget() {
        let d = HeadlessStepDeadline::from_scheduling_timeout_ms(60_000);
        let indices = vec![HeadlessRoundToolIdx::ServerToolCall(0)];
        let r = d.step_timeout_abort(
            &indices,
            0,
            &[json!({"id":"call-x","type":"function","function":{"name":"x","arguments":"{}"}})],
            |_| "y".into(),
        );
        assert!(r.is_none());
    }

    #[test]
    fn step_timeout_abort_fires_after_zero_budget_and_delay() {
        let d = HeadlessStepDeadline::from_scheduling_timeout_ms(0);
        std::thread::sleep(Duration::from_millis(15));
        let indices = vec![HeadlessRoundToolIdx::ServerToolCall(0)];
        let server = vec![json!({
            "id": "call-read",
            "type": "function",
            "function": {"name":"read_file","arguments":"{}"}
        })];
        let r = d
            .step_timeout_abort(&indices, 0, &server, |_| "edge".into())
            .expect("deadline should elapse");
        assert_eq!(r.0, 1);
        assert_eq!(r.1, vec!["read_file".to_string()]);
    }
}
