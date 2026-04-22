//! Turn evaluation engine — computes success/quality from turn signals.
//!
//! Replaces the crude heuristic (tool_calls > 0 → success, 0.7 quality) with
//! a multi-signal evaluator that considers:
//! - Tool error rate (fraction of calls that failed)
//! - Empty/minimal tool output
//! - Stall/verdict events from TurnGuard
//! - Budget pressure
//! - Repeated tool calls (retry loops)
//! - Correction patterns in user follow-up

use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use serde_json::json;

use crate::chat_turn_heuristics::looks_like_live_query_with_context;

/// Signals detected during evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalSignal {
    /// Fraction of tool calls that errored (0.0 = all ok, 1.0 = all failed).
    ToolErrorRate(f64),
    /// At least one tool returned empty or minimal output (<10 bytes).
    EmptyToolOutput,
    /// TurnGuard detected a stall (name stall, intent drift, etc.).
    StallDetected,
    /// Budget pressure exceeded 0.8 (agent struggling to fit in budget).
    HighBudgetPressure,
    /// Same tool/target called 3+ times — possible retry loop.
    RepeatToolCall(String),
    /// TurnGuard issued a warning or higher verdict.
    VerdictWarning,
    /// No tools used despite likely needing them.
    NoToolsNeeded,
    /// All tools succeeded with good output.
    AllToolsHealthy,
}

/// Result of evaluating a turn's success and quality.
#[derive(Debug, Clone)]
pub struct TurnEvaluation {
    /// Whether the turn is considered successful.
    pub success: bool,
    /// Quality score 0.0–1.0.
    pub quality: f64,
    /// How confident we are in this evaluation (0.0–1.0).
    pub confidence: f64,
    /// Signals that contributed to the evaluation.
    pub signals: Vec<EvalSignal>,
}

/// Per-tool-call record for evaluation (matches ToolCallRecord shape).
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub repeat_key: String,
    pub ok: bool,
    pub ms: u64,
    pub error: Option<String>,
    pub output_bytes: Option<u32>,
}

/// Evaluate a completed turn from its observable signals.
///
/// # Arguments
/// - `tool_calls` — per-tool-call audit records
/// - `stall_count` — number of stall events in this turn
/// - `verdict_warning` — whether TurnGuard issued a Warning or higher
/// - `budget_pressure` — 0.0–0.9 budget pressure from compaction tier
/// - `is_factual_query` — whether the query likely needed tool calls
pub fn evaluate_turn(
    tool_calls: &[ToolCallInfo],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    is_factual_query: bool,
) -> TurnEvaluation {
    let mut signals = Vec::new();
    let mut quality = 0.5_f64; // base quality
    let mut confidence = 0.5_f64; // base confidence

    let total_calls = tool_calls.len();

    // ─── No tool calls ──────────────────────────────────────────────────
    if total_calls == 0 {
        if is_factual_query {
            // Needed tools but didn't use any — bad
            signals.push(EvalSignal::NoToolsNeeded);
            return TurnEvaluation {
                success: false,
                quality: 0.2,
                confidence: 0.7,
                signals,
            };
        }
        // Conversational turn — no tools expected
        return TurnEvaluation {
            success: true,
            quality: 0.5,
            confidence: 0.4, // low confidence for text-only turns
            signals,
        };
    }

    // ─── Tool error analysis ────────────────────────────────────────────
    let error_count = tool_calls.iter().filter(|tc| !tc.ok).count();
    let error_rate = error_count as f64 / total_calls as f64;
    signals.push(EvalSignal::ToolErrorRate(error_rate));

    if error_rate == 0.0 {
        // All tools succeeded
        quality += 0.3;
        confidence += 0.2;
        signals.push(EvalSignal::AllToolsHealthy);
    } else if error_rate < 0.5 {
        // Some errors but mostly ok
        quality += 0.1;
    } else {
        // Majority errors
        quality -= 0.2;
        confidence += 0.1; // more confident this is bad
    }

    // ─── Empty output detection ─────────────────────────────────────────
    let empty_outputs = tool_calls
        .iter()
        .filter(|tc| tc.ok && tc.output_bytes.is_some_and(|b| b < 10))
        .count();
    if empty_outputs > 0 {
        signals.push(EvalSignal::EmptyToolOutput);
        quality -= 0.1 * (empty_outputs as f64 / total_calls as f64);
    }

    // ─── Repeat tool detection (retry loops) ────────────────────────────
    let mut call_counts: std::collections::HashMap<&str, (&str, usize)> =
        std::collections::HashMap::new();
    for tc in tool_calls {
        let entry = call_counts
            .entry(tc.repeat_key.as_str())
            .or_insert((tc.name.as_str(), 0));
        entry.1 += 1;
    }
    for (name, count) in call_counts.values() {
        if *count >= 3 {
            signals.push(EvalSignal::RepeatToolCall((*name).to_string()));
            quality -= 0.15;
        }
    }

    // ─── Stall events ───────────────────────────────────────────────────
    if stall_count > 0 {
        signals.push(EvalSignal::StallDetected);
        quality -= 0.1 * stall_count.min(3) as f64;
        confidence += 0.1;
    }

    // ─── TurnGuard verdict ──────────────────────────────────────────────
    if verdict_warning {
        signals.push(EvalSignal::VerdictWarning);
        quality -= 0.15;
        confidence += 0.1;
    }

    // ─── Budget pressure ────────────────────────────────────────────────
    if budget_pressure > 0.8 {
        signals.push(EvalSignal::HighBudgetPressure);
        quality -= 0.05;
    }

    // ─── Determine success ──────────────────────────────────────────────
    let success = error_rate < 0.5 && quality > 0.3;

    TurnEvaluation {
        success,
        quality: quality.clamp(0.0, 1.0),
        confidence: confidence.clamp(0.0, 1.0),
        signals,
    }
}

pub fn evaluate_tool_call_records(
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
) -> TurnEvaluation {
    // Synthetic placeholders (skill skipped/deferred, surgically removed
    // parallel tool calls) are audit-only records that do NOT represent real
    // tool execution. Filtering them here is the single choke-point that
    // keeps tool_error_rate, RepeatToolCall, EmptyToolOutput, and the
    // success/quality verdict honest.
    let tool_calls = tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .map(|record| ToolCallInfo {
            name: record.name.clone(),
            repeat_key: record
                .args_preview
                .as_deref()
                .map(str::trim)
                .filter(|preview| !preview.is_empty())
                .map(|preview| format!("{}::{preview}", record.name))
                .unwrap_or_else(|| record.name.clone()),
            ok: record.ok,
            ms: record.ms,
            error: record.error.clone(),
            output_bytes: record.output_bytes,
        })
        .collect::<Vec<_>>();
    let is_live_query = looks_like_live_query_with_context(input, recent_tools);
    evaluate_turn(
        &tool_calls,
        stall_count,
        verdict_warning,
        budget_pressure,
        is_live_query,
    )
}

pub fn eval_signal_to_json(signal: &EvalSignal) -> serde_json::Value {
    match signal {
        EvalSignal::ToolErrorRate(rate) => json!({
            "kind": "tool_error_rate",
            "weight": rate,
        }),
        EvalSignal::EmptyToolOutput => json!({
            "kind": "empty_tool_output",
            "message": "At least one successful tool call returned minimal output",
        }),
        EvalSignal::StallDetected => json!({
            "kind": "stall_detected",
            "message": "TurnGuard recorded a stall or divergence event",
        }),
        EvalSignal::HighBudgetPressure => json!({
            "kind": "high_budget_pressure",
            "message": "Budget pressure crossed the high-pressure threshold",
        }),
        EvalSignal::RepeatToolCall(name) => json!({
            "kind": "repeat_tool_call",
            "tool": name,
            "message": format!("Repeated tool call pattern detected for `{name}`"),
        }),
        EvalSignal::VerdictWarning => json!({
            "kind": "verdict_warning",
            "message": "TurnGuard emitted a warning-or-higher verdict",
        }),
        EvalSignal::NoToolsNeeded => json!({
            "kind": "missing_needed_tools",
            "message": "The turn looked like a live query but completed without tool calls",
        }),
        EvalSignal::AllToolsHealthy => json!({
            "kind": "all_tools_healthy",
            "message": "All tool calls completed successfully with non-empty output",
        }),
    }
}

pub fn eval_signals_to_json(signals: &[EvalSignal]) -> Vec<serde_json::Value> {
    signals.iter().map(eval_signal_to_json).collect()
}

#[allow(clippy::too_many_arguments)]
pub fn build_turn_evaluation_journal_event(
    session_id: Option<&str>,
    turn: Option<u32>,
    source: &str,
    input: &str,
    recent_tools: &[String],
    tool_call_records: &[ToolCallRecord],
    stall_count: usize,
    verdict_warning: bool,
    budget_pressure: f64,
    eval: &TurnEvaluation,
) -> JournalEvent {
    JournalEvent::turn_evaluation(
        session_id,
        turn,
        source,
        looks_like_live_query_with_context(input, recent_tools),
        eval.success,
        eval.quality,
        eval.confidence,
        budget_pressure,
        stall_count,
        verdict_warning,
        // Exclude synthetic placeholders (surgical removals, skipped skills) from
        // the user-visible tool_call_count — they are audit-only records.
        tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .count(),
        eval_signals_to_json(&eval.signals),
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalEventType;

    fn ok_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(500),
        }
    }

    fn err_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: false,
            ms: 50,
            error: Some("tool error".to_string()),
            output_bytes: None,
        }
    }

    fn empty_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            repeat_key: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(0),
        }
    }

    fn journal_ok_call(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: Some("ok".to_string()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn all_tools_succeed_high_quality() {
        let calls = vec![ok_call("bash"), ok_call("grep"), ok_call("read_file")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.success);
        assert!(eval.quality > 0.7, "quality={}", eval.quality);
        assert!(eval.signals.contains(&EvalSignal::AllToolsHealthy));
    }

    #[test]
    fn all_tools_fail_low_quality() {
        let calls = vec![err_call("bash"), err_call("grep")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(!eval.success);
        assert!(eval.quality < 0.4, "quality={}", eval.quality);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::ToolErrorRate(r) if *r > 0.9))
        );
    }

    #[test]
    fn mixed_success_moderate_quality() {
        let calls = vec![ok_call("bash"), err_call("grep"), ok_call("read_file")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.success); // error rate < 0.5
        let rate_signal = eval
            .signals
            .iter()
            .find(|s| matches!(s, EvalSignal::ToolErrorRate(_)));
        assert!(rate_signal.is_some());
    }

    #[test]
    fn no_tools_conversational_ok() {
        let eval = evaluate_turn(&[], 0, false, 0.3, false);
        assert!(eval.success);
        assert_eq!(eval.quality, 0.5);
        assert!(eval.confidence < 0.5); // low confidence for text-only
    }

    #[test]
    fn no_tools_factual_query_bad() {
        let eval = evaluate_turn(&[], 0, false, 0.3, true);
        assert!(!eval.success);
        assert!(eval.quality < 0.3);
        assert!(eval.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn stalls_reduce_quality() {
        let calls = vec![ok_call("bash")];
        let no_stall = evaluate_turn(&calls, 0, false, 0.3, false);
        let with_stall = evaluate_turn(&calls, 2, false, 0.3, false);
        assert!(with_stall.quality < no_stall.quality);
        assert!(with_stall.signals.contains(&EvalSignal::StallDetected));
    }

    #[test]
    fn verdict_warning_reduces_quality() {
        let calls = vec![ok_call("bash")];
        let no_verdict = evaluate_turn(&calls, 0, false, 0.3, false);
        let with_verdict = evaluate_turn(&calls, 0, true, 0.3, false);
        assert!(with_verdict.quality < no_verdict.quality);
        assert!(with_verdict.signals.contains(&EvalSignal::VerdictWarning));
    }

    #[test]
    fn high_budget_pressure_penalizes() {
        let calls = vec![ok_call("bash")];
        let low_pressure = evaluate_turn(&calls, 0, false, 0.3, false);
        let high_pressure = evaluate_turn(&calls, 0, false, 0.85, false);
        assert!(high_pressure.quality < low_pressure.quality);
        assert!(
            high_pressure
                .signals
                .contains(&EvalSignal::HighBudgetPressure)
        );
    }

    #[test]
    fn repeat_tool_calls_penalize() {
        let calls = vec![
            ok_call("bash"),
            ok_call("bash"),
            ok_call("bash"),
            ok_call("grep"),
        ];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(n) if n == "bash"))
        );
    }

    #[test]
    fn empty_output_penalizes() {
        let calls = vec![empty_call("read_file"), ok_call("bash")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.signals.contains(&EvalSignal::EmptyToolOutput));
        let all_ok = evaluate_turn(
            &[ok_call("read_file"), ok_call("bash")],
            0,
            false,
            0.3,
            false,
        );
        assert!(eval.quality < all_ok.quality);
    }

    #[test]
    fn quality_clamped_to_bounds() {
        // Worst case: all errors + stalls + verdict + pressure
        let calls = vec![err_call("a"), err_call("b"), err_call("c")];
        let eval = evaluate_turn(&calls, 5, true, 0.9, true);
        assert!(eval.quality >= 0.0);
        assert!(eval.quality <= 1.0);
        assert!(eval.confidence >= 0.0);
        assert!(eval.confidence <= 1.0);
    }

    #[test]
    fn confidence_increases_with_more_signals() {
        let simple = evaluate_turn(&[ok_call("bash")], 0, false, 0.3, false);
        let complex = evaluate_turn(&[err_call("bash"), err_call("grep")], 2, true, 0.9, false);
        assert!(complex.confidence > simple.confidence);
    }

    #[test]
    fn evaluate_tool_call_records_reuses_live_query_heuristic() {
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git_status".to_string()],
            &[],
            0,
            false,
            0.2,
        );
        assert!(!eval.success);
        assert!(eval.signals.contains(&EvalSignal::NoToolsNeeded));
    }

    #[test]
    fn build_turn_evaluation_journal_event_serializes_normalized_signals() {
        let records = vec![journal_ok_call("git_status")];
        let eval = evaluate_tool_call_records(
            "Check the latest git status",
            &["git_status".to_string()],
            &records,
            0,
            false,
            0.2,
        );

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(2),
            "cli_repl",
            "Check the latest git status",
            &["git_status".to_string()],
            &records,
            0,
            false,
            0.2,
            &eval,
        );

        assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    fn evaluate_records_skips_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        let rec = |name: &str, ok: bool, preview: Option<&str>| ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: preview.map(ToString::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // 1 real successful tool + 3 synthetic placeholders (skipped,
        // deferred, surgically_removed). A naive counter would see
        // error_rate = 3/4 = 0.75 and quality collapse. With filtering,
        // only the real success is counted and the turn scores as healthy.
        let records = vec![
            rec("git_show", true, Some("diff contents here")),
            rec(
                SURGICAL_REMOVAL_TOOL_NAME,
                true,
                Some("(removed from context — skill covered this work)"),
            ),
            rec("read_file", false, Some("Skipped: skill routed")),
            rec("read_file", false, Some("Deferred: skill invoked")),
        ];

        let eval = evaluate_tool_call_records(
            "review commit 179afcb",
            &["git_show".to_string()],
            &records,
            0,
            false,
            0.1,
        );

        // Must contain exactly one error_rate=0.0 signal AND AllToolsHealthy
        // — proves surgical/skipped/deferred were filtered before analysis.
        let has_zero_error = eval.signals.iter().any(
            |s| matches!(s, EvalSignal::ToolErrorRate(rate) if (*rate - 0.0).abs() < f64::EPSILON),
        );
        assert!(
            has_zero_error,
            "expected ToolErrorRate(0.0) after filtering synthetic placeholders, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::AllToolsHealthy)),
            "expected AllToolsHealthy signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.success,
            "turn with one real success and synthetic placeholders must be success=true"
        );
        assert!(
            eval.quality > 0.6,
            "quality should be high after filtering, got {}",
            eval.quality
        );
    }

    #[test]
    fn repeat_tool_call_ignores_surgical_removals() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // Four surgically_removed records in a single turn used to surface
        // as RepeatToolCall("(surgically_removed)") — a false "retry loop"
        // signal. After filtering they must not appear at all.
        let records: Vec<ToolCallRecord> = (0..4)
            .map(|_| ToolCallRecord {
                name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
                ok: true,
                ms: 0,
                error: None,
                input_bytes: None,
                output_bytes: Some(0),
                args_preview: None,
                result_preview: Some("(removed from context — skill covered this work)".into()),
                file_path: None,
                surgically_removed: Some(true),
                original_tool_name: Some("read_file".to_string()),
                ..Default::default()
            })
            .chain(std::iter::once(ToolCallRecord {
                name: "git_show".to_string(),
                ok: true,
                ms: 20,
                error: None,
                input_bytes: None,
                output_bytes: Some(400),
                args_preview: None,
                result_preview: Some("diff".into()),
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            }))
            .collect();

        let eval = evaluate_tool_call_records("noop", &[], &records, 0, false, 0.1);
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(_))),
            "no RepeatToolCall signal should be emitted for synthetic placeholders, got {:?}",
            eval.signals
        );
    }

    #[test]
    fn repeat_tool_call_distinguishes_distinct_args_preview() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        let distinct_greps = vec![
            record("grep", "/catch_tool_execution_panic/ in ."),
            record("grep", "/CLOUD_APPROVAL_REQUIRED_TOOLS/ in ."),
            record(
                "grep",
                "/struct ToolExecutionOutcome/ in rust/crates/astra-cli/src",
            ),
            record("grep", "/struct ToolExecutionOutcome/ in ."),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["grep".to_string()],
            &distinct_greps,
            0,
            false,
            0.1,
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not be treated as a retry loop: {:?}",
            eval.signals
        );

        let repeated_git_show = vec![
            record("git_show", "6f2f96e"),
            record("git_show", "6f2f96e"),
            record("git_show", "6f2f96e"),
        ];
        let eval = evaluate_tool_call_records(
            "review latest commit",
            &["git_show".to_string()],
            &repeated_git_show,
            0,
            false,
            0.1,
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git_show")),
            "identical git_show targets should still surface as repeat loops: {:?}",
            eval.signals
        );
    }

    #[test]
    fn real_session_0ac769_pattern_surfaces_git_show_and_read_file_repeats() {
        use astra_services::session_journal::ToolCallRecord;

        let record = |name: &str, args_preview: &str| ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 20,
            error: None,
            input_bytes: None,
            output_bytes: Some(120),
            args_preview: Some(args_preview.to_string()),
            result_preview: Some("ok".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        // Real session 0ac7696c had 7 LLM rounds with this 12-tool pattern:
        // git_show, git_show, read_file x3 + grep, grep x3 + read_file, git_show, git_show.
        // The persisted turn_evaluation surfaced repeat loops for read_file and
        // git_show, while distinct grep queries stayed healthy.
        let records = vec![
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record("grep", r#"/factual retry/ in rust/crates/runtime/src"#),
            record("grep", r#"/ContinueLoop/ in rust/crates/runtime/src"#),
            record("grep", r#"/TPM/ in rust/crates/runtime/src"#),
            record(
                "read_file",
                r#"{"path":"rust/crates/runtime/src/server/run_lifecycle.rs"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record(
                "git_show",
                r#"{"rev":"b273c589a73799070a71f4cfc6d55349b534d8d1"}"#,
            ),
            record("grep", r#"/turn_evaluation/ in rust/crates/runtime/src"#),
        ];

        let eval = evaluate_tool_call_records(
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git_show".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
        );

        assert!(
            eval.success,
            "real-session loop still completed successfully"
        );
        assert_eq!(eval.quality, 0.5);
        assert_eq!(eval.confidence, 0.7);
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "read_file")),
            "expected read_file repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            eval.signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "git_show")),
            "expected git_show repeat signal, got {:?}",
            eval.signals
        );
        assert!(
            !eval
                .signals
                .iter()
                .any(|s| matches!(s, EvalSignal::RepeatToolCall(name) if name == "grep")),
            "distinct grep queries should not collapse into a repeat loop: {:?}",
            eval.signals
        );

        let event = build_turn_evaluation_journal_event(
            Some("0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0"),
            Some(1),
            "cli_repl",
            "review b273c589a73799070a71f4cfc6d55349b534d8d1",
            &[
                "git_show".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
            ],
            &records,
            0,
            false,
            0.446_225,
            &eval,
        );
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 12);
        assert_eq!(metadata["signal_count"], 4);
        assert_eq!(metadata["quality"], 0.5);
        assert_eq!(metadata["confidence"], 0.7);
    }

    #[test]
    fn tool_call_count_excludes_synthetic_placeholders() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // 3 real tool calls + 2 surgical removals = 5 records total,
        // but tool_call_count should be 3 (only real).
        let real = || ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: Some("content".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let surgical = || ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(0),
            args_preview: None,
            result_preview: Some("(removed)".into()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("glob".to_string()),
            ..Default::default()
        };
        let records = vec![real(), surgical(), real(), surgical(), real()];

        let event = build_turn_evaluation_journal_event(
            Some("sess-1"),
            Some(1),
            "test-model",
            "do something",
            &[],
            &records,
            0,
            false,
            0.5,
            &TurnEvaluation {
                success: true,
                quality: 0.8,
                confidence: 0.9,
                signals: vec![],
            },
        );

        // Extract tool_call_count from the metadata JSON.
        let metadata = event.metadata.as_ref().expect("metadata should be present");
        let tool_call_count = metadata["tool_call_count"].as_u64().unwrap();
        assert_eq!(
            tool_call_count, 3,
            "tool_call_count must exclude synthetic placeholders, got {}",
            tool_call_count
        );
    }

    #[test]
    fn is_synthetic_placeholder_via_flag() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // New-style: surgically_removed flag set
        let flagged = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("bash".to_string()),
            ..Default::default()
        };
        assert!(flagged.is_synthetic_placeholder());

        // Backward-compat: legacy sentinel name only (no flag)
        let legacy = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(legacy.is_synthetic_placeholder());

        // Normal tool call: neither flag nor sentinel name
        let normal = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!normal.is_synthetic_placeholder());
    }
}
