//! Final turn reporting, status lines, and summary rendering.

use std::time::{Duration, Instant};

use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::{StreamResult, UsageAttribution};
use astra_services::session_journal;
use astra_turn_core::evaluation::TurnEvaluation;
use crossterm::style::Stylize;

/// Build a compact tool-call summary for cross-turn context continuity.
///
/// Appended to the assistant text in history so the next turn's prompt
/// contains file paths and tool outcomes from the previous turn — without
/// storing the full tool_call / tool_result messages.
pub(crate) fn build_turn_tool_summary(records: &[session_journal::ToolCallRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }

    let mut files = Vec::new();
    let mut failed = Vec::new();
    for record in records {
        if !record.was_executed() {
            continue;
        }
        if record.ok
            && let Some(file_path) = record.file_path.as_deref()
            && !files.contains(&file_path)
        {
            files.push(file_path);
        }
        if !record.ok && !failed.contains(&record.name.as_str()) {
            failed.push(record.name.as_str());
        }
    }

    let mut parts = Vec::new();
    if !files.is_empty() {
        if files.len() <= 15 {
            parts.push(format!("files: {}", files.join(", ")));
        } else {
            parts.push(format!(
                "files: {} (+{} more)",
                files[..15].join(", "),
                files.len() - 15
            ));
        }
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", failed.join(", ")));
    }
    parts.push(format!(
        "tool_calls: {}",
        records
            .iter()
            .filter(|record| record.was_executed())
            .count()
    ));

    format!("\n\n[Turn context: {}]", parts.join(" | "))
}

/// Build the text stored in history: assistant response + optional tool summary.
pub(crate) fn build_history_text(
    full_text: &str,
    records: &[session_journal::ToolCallRecord],
) -> String {
    let summary = build_turn_tool_summary(records);
    if summary.is_empty() {
        return full_text.to_string();
    }
    format!("{full_text}{summary}")
}

pub(crate) fn print_turn_status_line(
    state: &SessionState,
    result: &StreamResult,
    evaluation: Option<&TurnEvaluation>,
    turn_start: Instant,
) {
    if state.tui_render_policy.is_some() {
        return;
    }
    let parts = compact_completion_parts(state, result, turn_start.elapsed());
    eprintln!("{}", format!("  ─ {} ─", parts.join(" · ")).dim());
    if let Some(error) = state
        .session_persistence_error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        eprintln!(
            "{}",
            format!("  ⚠ Session persistence degraded: {error}").yellow()
        );
    }

    if let Some(notice) = interruption_status_notice(result) {
        eprintln!("{}", format!("  ⚠ {notice}").yellow());
    }
    if let Some(notice) =
        evaluation.and_then(|evaluation| evaluation_status_notice_for_result(result, evaluation))
    {
        eprintln!("{}", format!("  ⚠ {notice}").yellow());
    }
    print_context_window_warning(result.budget_pressure);

    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(80);
    eprintln!("{}", "─".repeat(width.min(72)).dim());
}

fn compact_completion_parts(
    _state: &SessionState,
    result: &StreamResult,
    elapsed: Duration,
) -> Vec<String> {
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };

    let mut parts = vec![format!("{elapsed_str} total")];
    if let Some(ttft_ms) = result.ttft_ms.filter(|ttft_ms| *ttft_ms > 0) {
        parts.push(format!("ttft {}", format_duration_ms(ttft_ms)));
    }
    if let Some(usage) = primary_usage_summary(result) {
        parts.push(usage);
    }
    if result.tool_calls_count > 0 {
        parts.push(format!(
            "{} tool{}",
            result.tool_calls_count,
            if result.tool_calls_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    parts
}

/// Compact, user-facing primary-model usage. Auxiliary calls and session
/// lifetime totals belong to Explain Analyze; they must not be folded into
/// this per-turn marker.
pub(crate) fn format_primary_usage_summary(
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    observed: bool,
    complete: bool,
    input_complete: bool,
) -> Option<String> {
    let has_lane = tokens_in.is_some()
        || tokens_out.is_some()
        || cache_read_tokens.is_some()
        || cache_creation_tokens.is_some();
    if !observed && !has_lane {
        return None;
    }
    if !has_lane {
        return Some("main usage unavailable".to_string());
    }

    let total = tokens_in
        .unwrap_or(0)
        .saturating_add(tokens_out.unwrap_or(0))
        .saturating_add(cache_read_tokens.unwrap_or(0))
        .saturating_add(cache_creation_tokens.unwrap_or(0));
    let mut summary = format!("{} tokens", format_usage_count(total, complete));
    if let Some(cache) = format_cache_usage_summary(
        tokens_in,
        cache_read_tokens,
        cache_creation_tokens,
        input_complete,
    ) {
        summary.push_str(&format!(" · {cache}"));
    }
    Some(summary)
}

/// Shared CLI/TUI cache presentation.
///
/// A complete attribution can report the whole-turn cache rate. A partial
/// attribution still has useful information when both fresh input and cache
/// reads are known: show the rate over the observed input lanes and scope it
/// explicitly to `known input`. This keeps the useful percentage visible
/// without presenting an incomplete turn as an exact whole-turn measurement.
pub(crate) fn format_cache_usage_summary(
    fresh: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    complete: bool,
) -> Option<String> {
    let cache_read = cache_read?;
    let Some(fresh) = fresh else {
        return Some(format!(
            "{} cached",
            format_usage_count(cache_read, complete)
        ));
    };

    let known_input = u128::from(fresh)
        .saturating_add(u128::from(cache_read))
        .saturating_add(u128::from(cache_creation.unwrap_or(0)));
    if let Some(percent) = (u128::from(cache_read) * 100).checked_div(known_input) {
        if complete && cache_creation.is_some() {
            return Some(format!("{percent}% cached"));
        }
        return Some(format!(
            "{} cached · {percent}% of known input",
            format_usage_count(cache_read, complete)
        ));
    }

    Some(format!(
        "{} cached",
        format_usage_count(cache_read, complete)
    ))
}

/// Partial sums are lower bounds. Truncate their compact decimal so rounding
/// cannot advertise a larger lower bound than the provider actually reported.
pub(crate) fn format_usage_count(tokens: u64, complete: bool) -> String {
    if complete {
        return format_token_count(tokens);
    }
    let count = if tokens >= 1_000_000 {
        format!(
            "{}.{:01}M",
            tokens / 1_000_000,
            tokens % 1_000_000 / 100_000
        )
    } else if tokens >= 1_000 {
        format!("{}.{:01}k", tokens / 1_000, tokens % 1_000 / 100)
    } else {
        tokens.to_string()
    };
    format!("≥{count}")
}

/// The only usage facts eligible for the compact primary-model summary. Raw
/// counters without attribution remain observable, but are deliberately not
/// presented as belonging to the selected model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrimaryUsageProjection {
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub observed: bool,
    pub complete: bool,
    pub input_complete: bool,
}

pub(crate) fn project_primary_usage(
    attribution: &UsageAttribution,
    raw_usage_observed: bool,
) -> PrimaryUsageProjection {
    let primary = attribution.primary;
    PrimaryUsageProjection {
        fresh_input_tokens: primary.and_then(|usage| usage.fresh_input_tokens),
        cache_read_tokens: primary.and_then(|usage| usage.cache_read_tokens),
        cache_creation_tokens: primary.and_then(|usage| usage.cache_creation_tokens),
        output_tokens: primary.and_then(|usage| usage.output_tokens),
        observed: attribution.has_observed_state() || raw_usage_observed,
        complete: primary.is_some() && attribution.primary_complete,
        input_complete: primary.is_some() && attribution.primary_input_complete,
    }
}

fn primary_usage_summary(result: &StreamResult) -> Option<String> {
    let attribution = &result.usage_attribution;
    let raw_has_values = result.prompt_tokens > 0
        || result.completion_tokens > 0
        || result.cache_read_tokens > 0
        || result.cache_creation_tokens > 0;
    let capture_observed =
        result.token_usage_coverage.attempts > 0 || result.token_usage_coverage.unavailable > 0;
    let projection = project_primary_usage(attribution, raw_has_values || capture_observed);
    format_primary_usage_summary(
        projection.fresh_input_tokens,
        projection.output_tokens,
        projection.cache_read_tokens,
        projection.cache_creation_tokens,
        projection.observed,
        projection.complete,
        projection.input_complete,
    )
}

fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{:.1}s", ms as f64 / 1_000.0)
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// A typed terminal result owns the user-visible completion state.  The raw
/// tool ledger remains useful evidence, but it must not reopen a completed
/// assessment after runtime reconciliation has already accepted it.  When a
/// turn is interrupted or explicitly marked unverified, use the semantic
/// record-aware evaluator to explain the remaining obligation.
fn evaluation_status_notice_for_result(
    result: &StreamResult,
    evaluation: &TurnEvaluation,
) -> Option<String> {
    if result.final_state == "completed" && !result.server_terminal_unverified {
        return None;
    }
    astra_turn_core::evaluation::turn_evaluation_status_notice_for_records(
        evaluation,
        &result.tool_call_records,
    )
}

pub(crate) fn interruption_status_notice(result: &StreamResult) -> Option<String> {
    let interruption = result.interruption.as_ref()?;
    if let Some(user_message) = interruption
        .get("user_message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        let has_distinct_answer =
            !result.full_text.trim().is_empty() && result.full_text.trim() != user_message;
        return Some(
            if result.interruption_kind.as_deref() == Some("execution_incomplete")
                && has_distinct_answer
            {
                format!("Partial answer shown. {user_message}")
            } else {
                user_message.to_string()
            },
        );
    }

    let kind = result
        .interruption_kind
        .as_deref()
        .or_else(|| interruption.get("kind").and_then(serde_json::Value::as_str))?;
    let resumable = interruption
        .get("resumable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let suffix = if resumable {
        " You can continue in the next message."
    } else {
        ""
    };
    Some(format!("[{kind}] Turn interrupted.{suffix}"))
}

/// Print a context window warning when budget pressure exceeds thresholds.
pub(crate) fn print_context_window_warning(budget_pressure: f64) {
    const WARNING_THRESHOLD: f64 = 0.70;
    const CRITICAL_THRESHOLD: f64 = 0.85;

    if budget_pressure >= CRITICAL_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🔴 Context window {:.0}% full ({:.0}% remaining) — consider /compact or starting a new session",
                budget_pressure * 100.0,
                remaining
            )
            .red()
        );
    } else if budget_pressure >= WARNING_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🟡 Context window {:.0}% used ({:.0}% remaining) — use /stats context for details",
                budget_pressure * 100.0,
                remaining
            )
            .yellow()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_history_text, build_turn_tool_summary, compact_completion_parts,
        evaluation_status_notice_for_result, interruption_status_notice,
    };
    use astra_services::session_journal;
    use astra_turn_core::evaluation::{
        EvalSignal, EvaluationThresholds, TurnEvaluation, turn_evaluation_status_notice,
    };
    use std::time::Duration;

    fn make_record(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            file_path: file_path.map(|path| path.into()),
            ..Default::default()
        }
    }

    #[test]
    fn turn_evaluation_notice_reports_unresolved_outcome_failure() {
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        let notice = turn_evaluation_status_notice(&eval).expect("notice");
        assert!(notice.contains("test_failure x1"));
        assert!(notice.contains("incomplete"));
    }

    #[test]
    fn turn_evaluation_notice_ignores_successful_turn() {
        let eval = TurnEvaluation {
            success: true,
            quality: 0.8,
            confidence: 0.7,
            signals: vec![EvalSignal::AllToolsHealthy],
            thresholds: EvaluationThresholds::default(),
        };

        assert!(turn_evaluation_status_notice(&eval).is_none());
    }

    #[test]
    fn completed_typed_terminal_does_not_reopen_assessment_failure() {
        let mut result = crate::tests::stub_stream_result("");
        result.final_state = "completed".into();
        result.server_terminal_unverified = false;
        let mut failed = make_record("bash", false, None);
        failed.args_full =
            Some(serde_json::json!({"command": "cargo test --test artifact"}).to_string());
        failed.result_class = Some("test_failure".into());
        result.tool_call_records = vec![failed];
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        assert!(
            evaluation_status_notice_for_result(&result, &eval).is_none(),
            "accepted runtime settlement must not be reclassified from raw evidence"
        );
    }

    #[test]
    fn interrupted_unverified_terminal_keeps_validation_failure_visible() {
        let mut result = crate::tests::stub_stream_result("");
        result.final_state = "interrupted".into();
        result.server_terminal_unverified = true;
        let mut failed = make_record("bash", false, None);
        failed.args_full =
            Some(serde_json::json!({"command": "cargo test --test artifact"}).to_string());
        failed.result_class = Some("test_failure".into());
        result.tool_call_records = vec![failed];
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        let notice = evaluation_status_notice_for_result(&result, &eval).expect("notice");
        assert!(notice.contains("test_failure x1"));
    }

    #[test]
    fn interruption_status_notice_prefers_user_message() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "user_message": "[budget_exhausted] 2 tool call(s) completed. Continue next turn."
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[budget_exhausted] 2 tool call(s) completed. Continue next turn.")
        );
    }

    #[test]
    fn interruption_status_notice_falls_back_to_kind_and_resumable_hint() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption_kind = Some("context_budget".into());
        result.interruption = Some(serde_json::json!({
            "kind": "context_budget",
            "resumable": true
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[context_budget] Turn interrupted. You can continue in the next message.")
        );
    }

    #[test]
    fn interruption_status_notice_labels_answer_separately_from_unverified_execution() {
        let mut result = crate::tests::stub_stream_result("partial answer");
        result.interruption_kind = Some("execution_incomplete".into());
        result.interruption = Some(serde_json::json!({
            "kind": "execution_incomplete",
            "resumable": true,
            "user_message": "Execution did not reach a verified terminal state. Review the available execution evidence, then continue to reconcile the unfinished work."
        }));

        let notice = interruption_status_notice(&result).expect("notice");
        assert!(notice.starts_with("Partial answer shown."));
        assert!(notice.contains("verified terminal state"));
        assert_eq!(result.full_text, "partial answer");
    }

    #[test]
    fn unclassified_overall_is_not_presented_as_primary_usage() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.model = Some("deepseek-flash".into());
        let mut result = crate::tests::stub_stream_result("answer");
        result.prompt_tokens = 87_900;
        result.cache_read_tokens = 75_000;
        result.completion_tokens = 9;
        result.tool_calls_count = 1;

        assert_eq!(
            compact_completion_parts(&state, &result, Duration::from_millis(8_500)),
            vec![
                "8.5s total".to_string(),
                "main usage unavailable".to_string(),
                "1 tool".to_string()
            ]
        );
    }

    #[test]
    fn complete_primary_metrics_show_cache_rate_but_not_auxiliary_details() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.model = Some("deepseek-flash".into());
        let mut result = crate::tests::stub_stream_result("answer");
        result.ttft_ms = Some(1_250);
        result.usage_attribution.primary =
            Some(crate::cli::stream::streaming_types::AttributedTokenUsage {
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(0),
                output_tokens: Some(9),
            });
        result.usage_attribution.primary_complete = true;
        result.usage_attribution.primary_input_complete = true;
        result.usage_attribution.primary_model = Some("deepseek-flash".into());

        assert_eq!(
            compact_completion_parts(&state, &result, Duration::from_millis(5_200)),
            vec![
                "5.2s total".to_string(),
                "ttft 1.2s".to_string(),
                "1.0k tokens · 90% cached".to_string(),
            ]
        );
    }

    #[test]
    fn incomplete_primary_metrics_show_a_scoped_cache_rate() {
        let state = crate::cli::session::session_state::SessionState::default();
        let mut result = crate::tests::stub_stream_result("answer");
        result.usage_attribution.primary =
            Some(crate::cli::stream::streaming_types::AttributedTokenUsage {
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(0),
                output_tokens: Some(20),
            });
        result.usage_attribution.primary_complete = false;

        let parts = compact_completion_parts(&state, &result, Duration::from_millis(5_200));
        assert!(parts.contains(&"≥1.0k tokens · ≥900 cached · 90% of known input".to_string()));
    }

    #[test]
    fn observed_partial_usage_keeps_the_session_cache_rate_visible() {
        assert_eq!(
            super::format_primary_usage_summary(
                Some(10_392),
                Some(926),
                Some(34_944),
                Some(0),
                true,
                false,
                false,
            )
            .as_deref(),
            Some("≥46.2k tokens · ≥34.9k cached · 77% of known input")
        );
    }

    #[test]
    fn primary_attempt_without_usage_is_not_presented_as_zero() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.model = Some("deepseek-flash".into());
        let mut result = crate::tests::stub_stream_result("answer");
        result.usage_attribution.primary_attempts = 1;
        result.usage_attribution.primary_model = Some("deepseek-flash".into());

        assert!(
            compact_completion_parts(&state, &result, Duration::from_millis(900))
                .contains(&"main usage unavailable".to_string())
        );
        assert!(
            !compact_completion_parts(&state, &result, Duration::from_millis(900))
                .iter()
                .any(|part| part.contains("tokens"))
        );
    }

    #[test]
    fn explicit_zero_cache_lane_renders_zero_percent() {
        assert_eq!(
            super::format_primary_usage_summary(
                Some(100),
                Some(9),
                Some(0),
                Some(0),
                true,
                true,
                true
            )
            .as_deref(),
            Some("109 tokens · 0% cached")
        );
    }

    #[test]
    fn complete_input_keeps_cache_rate_when_output_is_unknown() {
        assert_eq!(
            super::format_primary_usage_summary(
                Some(100),
                None,
                Some(900),
                Some(0),
                true,
                false,
                true
            )
            .as_deref(),
            Some("≥1.0k tokens · 90% cached")
        );
    }

    #[test]
    fn cache_summary_distinguishes_unknown_zero_partial_and_complete() {
        assert_eq!(super::format_usage_count(19_999, false), "≥19.9k");
        assert_eq!(super::format_usage_count(1_999_999, false), "≥1.9M");
        assert_eq!(super::format_usage_count(19_999, true), "20.0k");
        for (fresh, read, write, complete, expected) in [
            (Some(100), None, None, false, None),
            (
                Some(100),
                Some(0),
                None,
                false,
                Some("≥0 cached · 0% of known input"),
            ),
            (None, Some(900), None, false, Some("≥900 cached")),
            (
                Some(100),
                Some(900),
                Some(1000),
                false,
                Some("≥900 cached · 45% of known input"),
            ),
            (Some(100), Some(900), Some(1000), true, Some("45% cached")),
            (Some(0), Some(0), Some(0), true, Some("0 cached")),
            (
                Some(u64::MAX),
                Some(u64::MAX),
                Some(0),
                true,
                Some("50% cached"),
            ),
        ] {
            assert_eq!(
                super::format_cache_usage_summary(fresh, read, write, complete).as_deref(),
                expected
            );
        }
    }

    #[test]
    fn tool_summary_empty_when_no_tools() {
        let summary = build_turn_tool_summary(&[]);
        assert!(summary.is_empty());
    }

    #[test]
    fn tool_summary_lists_successful_files_touched() {
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("str_replace", true, Some("src/lib.rs")),
            make_record("read_file", true, Some("src/main.rs")),
        ];

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("files: src/main.rs, src/lib.rs"));
        assert!(summary.contains("tool_calls: 3"));
        assert!(!summary.contains("failed:"));
    }

    #[test]
    fn tool_summary_includes_failures_without_failed_paths() {
        let records = vec![
            make_record("read_file", false, Some("rust/astra/src/bridge/mod.rs")),
            make_record(
                "str_replace",
                true,
                Some("crates/runtime/src/bridge/mod.rs"),
            ),
        ];

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("files: crates/runtime/src/bridge/mod.rs"));
        assert!(summary.contains("failed: read_file"));
        assert!(
            !summary.contains("rust/astra"),
            "failed file paths must not be persisted into prompt-facing history: {summary}"
        );
    }

    #[test]
    fn tool_summary_caps_successful_files() {
        let mut records = Vec::new();
        for idx in 0..18 {
            records.push(make_record(
                "read_file",
                true,
                Some(&format!("src/file_{idx}.rs")),
            ));
        }
        records.push(make_record("edit", false, Some("rust/old/path.rs")));

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("failed: edit"));
        assert!(summary.contains("(+3 more)"));
        assert!(!summary.contains("rust/old/path.rs"));
    }

    #[test]
    fn tool_summary_stays_compact_under_heavy_load() {
        let mut records = Vec::new();
        for idx in 0..50 {
            let file = format!("src/module_{}/file_{}.rs", idx / 5, idx % 5);
            records.push(make_record(
                if idx % 3 == 0 {
                    "read_file"
                } else {
                    "str_replace"
                },
                idx % 7 != 0,
                Some(&file),
            ));
        }

        let summary = build_turn_tool_summary(&records);
        assert!(
            summary.len() < 2048,
            "summary should be compact, got {} bytes: {summary}",
            summary.len()
        );
        assert!(summary.contains("src/module_0/file_1.rs"));
        assert!(
            !summary.contains("src/module_0/file_0.rs"),
            "failed file paths must not be retained: {summary}"
        );
        assert!(
            summary.contains("more)"),
            "should truncate beyond 15 files: {summary}"
        );
    }

    #[test]
    fn history_text_appends_tool_summary() {
        let full_text = "Updated three files.";
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("edit", false, Some("src/lib.rs")),
        ];
        let history_text = build_history_text(full_text, &records);
        assert!(history_text.starts_with(full_text));
        assert!(history_text.contains("[Turn context:"));
        assert!(history_text.contains("failed: edit"));
    }

    #[test]
    fn history_text_noop_without_tool_summary() {
        let full_text = "No tools used.";
        assert_eq!(build_history_text(full_text, &[]), full_text);
    }
}
