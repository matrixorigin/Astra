//! Error/limit enrichment and TurnGuard quality for the headless tool round (CLI §5.5).

use std::time::{Duration, Instant};

use crate::guardrails::error_recovery::{
    ErrorCategory, build_recovery_message_with_evidence, classify_error,
};
use crate::guardrails::turn_guard::TurnGuard;
use crate::headless_tool_assembly::{HeadlessRoundToolIdx, headless_timeout_aborted_tool_names};
use crate::result_quality::ResultQuality;
use crate::tool::result::semantics::is_resource_limit_output;
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
    ResourceLimitDetectedInOutput { tool: String },
}

/// Mutable state used while enriching one headless tool result.
pub struct HeadlessOutputEnrichCtx<'a> {
    pub turn_guard: &'a mut TurnGuard,
}

/// Inputs and mutable output for enriching one headless tool result.
pub struct HeadlessOutputEnrichRequest<'a> {
    pub name: &'a str,
    pub result_str: &'a mut String,
    pub is_err: &'a mut bool,
    pub source_error_kind: Option<ErrorCategory>,
    pub source_recovery_evidence: Option<&'a astra_core::ToolFailureEvidence>,
    pub tool_already_restricted: bool,
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
    } = request;
    let mut resource_limit_recorded = false;
    let is_typed_wait = serde_json::from_str::<Value>(result_str)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "waiting")
        })
        .unwrap_or(false);

    if *is_err && !tool_already_restricted && !is_typed_wait {
        let category = source_error_kind.unwrap_or_else(|| classify_error(result_str.as_str()));

        if matches!(category, ErrorCategory::ResourceLimit) {
            ctx.turn_guard.health.record_resource_limit_failure(name);
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
            result_str.as_str(),
            category,
            &avoidance_advised,
            source_recovery_evidence,
        );
        result_str.push_str(&format!("\n{recovery_msg}"));
    }

    if !*is_err && !tool_already_restricted && is_resource_limit_output(result_str.as_str()) {
        ctx.turn_guard.health.record_resource_limit_failure(name);
        ctx.turn_guard
            .errors
            .record_error(ErrorCategory::ResourceLimit);
        *is_err = true;
        resource_limit_recorded = true;
        on_signal(HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput {
            tool: name.to_string(),
        });
    }

    resource_limit_recorded
}

/// Record tool result quality and append optional TurnGuard feedback into `result_str`.
pub fn append_headless_result_quality_feedback(
    name: &str,
    result_str: &mut String,
    source_error_kind: Option<ErrorCategory>,
    execution_failed: bool,
    resource_limit_recorded: bool,
    turn_guard: &mut TurnGuard,
) -> ResultQuality {
    let result_quality = if resource_limit_recorded {
        ResultQuality::Error
    } else if execution_failed {
        turn_guard.record_failed_tool_result_with_kind(name, result_str.as_str(), source_error_kind)
    } else {
        turn_guard.record_tool_result_with_kind(name, result_str.as_str(), source_error_kind)
    };
    // Execution errors already received classified recovery evidence in
    // `enrich_headless_tool_output_for_errors_and_limits`. Appending generic
    // quality feedback here creates a second, often contradictory instruction
    // (for example "try another tool" after a route-scoped transport failure).
    if !execution_failed
        && !resource_limit_recorded
        && let Some(feedback) = turn_guard.result_feedback(name, result_quality)
    {
        result_str.push_str(&format!("\n{feedback}"));
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
    /// Per-invocation, model-visible result that may receive a duplicate hint.
    pub result_str: &'a mut String,
    pub call_id: Option<&'a str>,
    pub turn_index: usize,
    pub semantic_context_generation: u64,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub step_recorder: &'a mut StepRecorder,
    pub semantic_dedup: &'a mut SemanticDedup,
}

/// After a successful cacheable tool: persist idempotency row, attach
/// to recorder, semantic hint.  Wrapper that no-ops when the tool
/// errored, so callers don't have to repeat the `!is_err` guard at
/// every site.
pub fn record_headless_cacheable_success_and_semantic_hint_if_ok(
    name: &str,
    args: &Value,
    idem_key: &IdempotencyKey,
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
    idem_key: &IdempotencyKey,
    ctx: HeadlessCacheableRecordCtx<'_>,
) {
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
    ctx.semantic_dedup
        .append_near_duplicate_hint_for_observation_with_generation(
            ctx.result_str,
            ctx.observation,
            name,
            args,
            ctx.turn_index,
            ctx.semantic_context_generation,
        );
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
        };
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "bash",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
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
        };

        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "read_file",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
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
    fn enrich_resource_limit_in_output_flips_err() {
        let mut tg = TurnGuard::new();
        let mut out = "fork: retry: Resource temporarily unavailable".to_string();
        let mut is_err = false;
        let mut signals = Vec::new();
        let mut ctx = HeadlessOutputEnrichCtx {
            turn_guard: &mut tg,
        };
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "bash",
                result_str: &mut out,
                is_err: &mut is_err,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
            },
            &mut ctx,
            |s| signals.push(s),
        );
        assert!(rec);
        assert!(is_err);
        assert_eq!(
            signals,
            vec![HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput {
                tool: "bash".into()
            }]
        );
    }

    #[test]
    fn typed_wait_remains_parseable_without_generic_failure_advice() {
        let mut turn_guard = TurnGuard::new();
        let mut output = json!({
            "status": "waiting",
            "agent_id": "reviewer-1",
            "reason": "executor_offline"
        })
        .to_string();
        let expected = output.clone();
        let mut is_error = true;
        let mut context = HeadlessOutputEnrichCtx {
            turn_guard: &mut turn_guard,
        };

        let resource_limit = enrich_headless_tool_output_for_errors_and_limits(
            HeadlessOutputEnrichRequest {
                name: "agent",
                result_str: &mut output,
                is_err: &mut is_error,
                source_error_kind: None,
                source_recovery_evidence: None,
                tool_already_restricted: false,
            },
            &mut context,
            |_| panic!("typed waiting is not a resource failure"),
        );

        assert!(!resource_limit);
        assert_eq!(output, expected);
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["status"],
            "waiting"
        );
    }

    #[test]
    fn source_authored_large_input_evidence_drives_targeted_recovery() {
        let mut turn_guard = TurnGuard::new();
        let mut output = "opaque external failure".to_string();
        let mut is_error = true;
        let mut context = HeadlessOutputEnrichCtx {
            turn_guard: &mut turn_guard,
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
            },
            &mut context,
            |_| {},
        );

        assert!(output.contains("targeted line/range read"), "{output}");
        assert!(!output.contains("retry the same tool"), "{output}");
    }

    #[test]
    fn append_feedback_after_success() {
        let mut tg = TurnGuard::new();
        let mut out = "ok".to_string();
        let _q =
            append_headless_result_quality_feedback("bash", &mut out, None, false, false, &mut tg);
        // May or may not append depending on classifier; string should remain valid UTF-8.
        assert!(!out.is_empty());
    }

    #[test]
    fn append_feedback_preserves_structured_execution_failure() {
        let mut tg = TurnGuard::new();
        let mut out = json!({
            "status": "failed",
            "error": "Unknown tool `outline`",
            "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
            "retryable": false
        })
        .to_string();
        out.push_str("\nadditional recovery guidance");

        let quality = append_headless_result_quality_feedback(
            "outline",
            &mut out,
            Some(astra_core::ErrorKind::ToolNotFound),
            true,
            false,
            &mut tg,
        );

        assert_eq!(quality, ResultQuality::Error);
        let health = tg.health.get("outline").expect("tool health");
        assert_eq!(health.total_failures, 1);
        assert_eq!(health.consecutive_failures, 1);
        assert!(
            !out.contains("Use another tool only"),
            "classified recovery must not receive a second generic error instruction: {out}"
        );
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
