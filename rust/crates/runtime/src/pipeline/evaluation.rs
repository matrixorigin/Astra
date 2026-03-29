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
    /// Same tool called 3+ times — possible retry loop.
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
        .filter(|tc| tc.ok && tc.output_bytes.map_or(false, |b| b < 10))
        .count();
    if empty_outputs > 0 {
        signals.push(EvalSignal::EmptyToolOutput);
        quality -= 0.1 * (empty_outputs as f64 / total_calls as f64);
    }

    // ─── Repeat tool detection (retry loops) ────────────────────────────
    let mut call_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for tc in tool_calls {
        *call_counts.entry(tc.name.as_str()).or_default() += 1;
    }
    for (name, count) in &call_counts {
        if *count >= 3 {
            signals.push(EvalSignal::RepeatToolCall(name.to_string()));
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(500),
        }
    }

    fn err_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            ok: false,
            ms: 50,
            error: Some("tool error".to_string()),
            output_bytes: None,
        }
    }

    fn empty_call(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            output_bytes: Some(0),
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
        assert!(eval
            .signals
            .iter()
            .any(|s| matches!(s, EvalSignal::ToolErrorRate(r) if *r > 0.9)));
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
        assert!(high_pressure
            .signals
            .contains(&EvalSignal::HighBudgetPressure));
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
        assert!(eval
            .signals
            .iter()
            .any(|s| matches!(s, EvalSignal::RepeatToolCall(n) if n == "bash")));
    }

    #[test]
    fn empty_output_penalizes() {
        let calls = vec![empty_call("read_file"), ok_call("bash")];
        let eval = evaluate_turn(&calls, 0, false, 0.3, false);
        assert!(eval.signals.contains(&EvalSignal::EmptyToolOutput));
        let all_ok = evaluate_turn(&[ok_call("read_file"), ok_call("bash")], 0, false, 0.3, false);
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
}
