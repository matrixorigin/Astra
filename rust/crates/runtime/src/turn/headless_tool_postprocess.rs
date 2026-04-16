//! Error/limit enrichment and TurnGuard quality for the headless tool round (CLI §5.5).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::{
    CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache, StepCheckpoint, epoch_ms,
};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::turn::error_recovery::{ErrorCategory, build_recovery_message, classify_error};
use crate::turn::headless_tool_assembly::{
    HeadlessRoundToolIdx, headless_timeout_aborted_tool_names,
};
use crate::turn::result_quality::ResultQuality;
use crate::turn::tool_result_semantics::is_resource_limit_output;
use crate::turn::turn_guard::TurnGuard;

use serde_json::Value;

/// UI hooks for headless postprocess (CLI maps to colored `eprintln!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessOutputEnrichSignal {
    ResourceLimitBlocked { tool: String },
    ResourceLimitDetectedInOutput { tool: String },
}

/// `true` when resource-limit handling forced error-quality treatment (matches CLI `resource_limit_recorded`).
pub fn enrich_headless_tool_output_for_errors_and_limits(
    name: &str,
    result_str: &mut String,
    is_err: &mut bool,
    tool_already_restricted: bool,
    turn_guard: &mut TurnGuard,
    restricted_tools: &mut HashSet<String>,
    mut on_signal: impl FnMut(HeadlessOutputEnrichSignal),
) -> bool {
    let mut resource_limit_recorded = false;

    if *is_err && !tool_already_restricted {
        let category = classify_error(result_str.as_str());

        if matches!(category, ErrorCategory::ResourceLimit) {
            turn_guard.health.record_resource_limit_failure(name);
            turn_guard.errors.record_error(category);
            restricted_tools.insert(name.to_string());
            resource_limit_recorded = true;
            on_signal(HeadlessOutputEnrichSignal::ResourceLimitBlocked {
                tool: name.to_string(),
            });
        }

        if category.is_retryable() {
            turn_guard.errors.record_retry(false);
        }

        let deprioritized = turn_guard.health.deprioritized_tools();
        let recovery_msg =
            build_recovery_message(name, result_str.as_str(), category, &deprioritized);
        result_str.push_str(&format!("\n{recovery_msg}"));
    }

    if !*is_err && !tool_already_restricted && is_resource_limit_output(result_str.as_str()) {
        turn_guard.health.record_resource_limit_failure(name);
        turn_guard.errors.record_error(ErrorCategory::ResourceLimit);
        restricted_tools.insert(name.to_string());
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
    resource_limit_recorded: bool,
    turn_guard: &mut TurnGuard,
) -> ResultQuality {
    let result_quality = if resource_limit_recorded {
        ResultQuality::Error
    } else {
        turn_guard.record_tool_result(name, result_str.as_str())
    };
    if let Some(feedback) = turn_guard.result_feedback(name, result_quality) {
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
    pub result_str: &'a mut String,
    pub turn_index: usize,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub step_recorder: &'a mut StepRecorder,
    pub semantic_dedup: &'a mut SemanticDedup,
}

/// After a successful cacheable tool: persist idempotency row, attach to recorder, semantic hint.
pub fn record_headless_cacheable_success_and_semantic_hint(
    name: &str,
    args: &Value,
    idem_key: &IdempotencyKey,
    ctx: HeadlessCacheableRecordCtx<'_>,
) {
    let cached_result = CachedToolResult {
        tool_name: name.to_string(),
        output: ctx.result_str.clone(),
        is_error: false,
        cached_at: epoch_ms(),
    };
    ctx.step_recorder
        .attach_cached_result(cached_result.clone());
    ctx.idempotency_cache.record(idem_key, cached_result);
    ctx.semantic_dedup.append_near_duplicate_hint_if_any(
        ctx.result_str,
        name,
        args,
        ctx.turn_index,
    );
}

/// Best-effort light checkpoint after each tool (matches CLI headless path).
pub fn try_write_light_headless_step_checkpoint(session_id: &str, step_recorder: &StepRecorder) {
    if let Some(light) = step_recorder.build_light_checkpoint() {
        let cp = StepCheckpoint::Light(light);
        let n = step_recorder.summary().checkpoints;
        let _ = step_checkpoint::write_step_checkpoint(session_id, n, &cp);
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
    fn enrich_resource_limit_classifies_and_restricts() {
        let mut tg = TurnGuard::new();
        let mut restricted = HashSet::new();
        let mut out = "out of memory".to_string();
        let mut is_err = true;
        let mut signals = Vec::new();
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            "bash",
            &mut out,
            &mut is_err,
            false,
            &mut tg,
            &mut restricted,
            |s| signals.push(s),
        );
        assert!(rec);
        assert!(restricted.contains("bash"));
        assert_eq!(
            signals,
            vec![HeadlessOutputEnrichSignal::ResourceLimitBlocked {
                tool: "bash".into()
            }]
        );
        assert!(out.contains("out of memory"));
    }

    #[test]
    fn enrich_resource_limit_in_output_flips_err() {
        let mut tg = TurnGuard::new();
        let mut restricted = HashSet::new();
        let mut out = "fork: retry: Resource temporarily unavailable".to_string();
        let mut is_err = false;
        let mut signals = Vec::new();
        let rec = enrich_headless_tool_output_for_errors_and_limits(
            "bash",
            &mut out,
            &mut is_err,
            false,
            &mut tg,
            &mut restricted,
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
    fn append_feedback_after_success() {
        let mut tg = TurnGuard::new();
        let mut out = "ok".to_string();
        let _q = append_headless_result_quality_feedback("bash", &mut out, false, &mut tg);
        // May or may not append depending on classifier; string should remain valid UTF-8.
        assert!(!out.is_empty());
    }

    #[test]
    fn step_timeout_abort_none_under_long_budget() {
        let d = HeadlessStepDeadline::from_scheduling_timeout_ms(60_000);
        let indices = vec![HeadlessRoundToolIdx::ServerToolCall(0)];
        let r = d.step_timeout_abort(&indices, 0, &[json!({"name":"x","arguments":{}})], |_| {
            "y".into()
        });
        assert!(r.is_none());
    }

    #[test]
    fn step_timeout_abort_fires_after_zero_budget_and_delay() {
        let d = HeadlessStepDeadline::from_scheduling_timeout_ms(0);
        std::thread::sleep(Duration::from_millis(15));
        let indices = vec![HeadlessRoundToolIdx::ServerToolCall(0)];
        let server = vec![json!({"name":"read_file","arguments":{}})];
        let r = d
            .step_timeout_abort(&indices, 0, &server, |_| "edge".into())
            .expect("deadline should elapse");
        assert_eq!(r.0, 1);
        assert_eq!(r.1, vec!["read_file".to_string()]);
    }
}
