//! Error/limit enrichment and TurnGuard quality for the headless tool round (CLI §5.5).

use std::collections::HashSet;
use std::time::Duration;

use crate::turn::error_recovery::{ErrorCategory, build_recovery_message, classify_error};
use crate::turn::result_quality::ResultQuality;
use crate::turn::tool_result_semantics::is_resource_limit_output;
use crate::turn::turn_guard::TurnGuard;

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

        if matches!(category, ErrorCategory::Transient) {
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
}
