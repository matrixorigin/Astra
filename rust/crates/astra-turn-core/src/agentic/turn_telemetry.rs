//! First-hit latency and selection-report capture for agentic turn telemetry (CLI wires fields).

use std::time::Instant;

use crate::tool::registry::report::SelectionReport;

/// Set `*slot` to elapsed ms since `start` only when `slot` is still `None`.
pub fn record_first_latency_ms_since(slot: &mut Option<u64>, start: Instant) {
    if slot.is_none() {
        *slot = Some(start.elapsed().as_millis() as u64);
    }
}

/// Persist the first non-empty selection report and track peak budget pressure.
/// The selection report is captured once (first non-empty call); budget pressure
/// is updated on every call, keeping the maximum observed value so that
/// turn/eval journal events reflect actual peak pressure, not stale initial 0.0.
pub fn capture_first_selection_report_if_empty(
    slot: &mut Option<SelectionReport>,
    peak_budget_pressure: &mut f64,
    report: SelectionReport,
    budget_pressure: f64,
) {
    if slot.is_none() {
        *slot = Some(report);
    }
    if budget_pressure > *peak_budget_pressure {
        *peak_budget_pressure = budget_pressure;
    }
}

/// Compact token count for verbose REPL footers (`42tok`, `12.3k`).
#[must_use]
pub fn format_token_count_compact(tokens: u64) -> String {
    if tokens < 1000 {
        format!("{tokens}tok")
    } else {
        format!("{:.1}k", tokens as f64 / 1000.0)
    }
}

/// Abbreviate session id for footer display (first 8 chars); `None` → `"?"`.
#[must_use]
pub fn session_id_footer_abbrev(session_id: Option<&str>) -> &str {
    match session_id {
        None => "?",
        Some(s) => match s.char_indices().nth(8) {
            None => s,
            Some((byte_idx, _)) => &s[..byte_idx],
        },
    }
}

/// Default `StepRecorder` run label for ephemeral chat streams (`chat-{ms}`).
#[must_use]
pub fn step_recorder_chat_ephemeral_run_id(loop_start_elapsed_ms: u128) -> String {
    format!("chat-{loop_start_elapsed_ms}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_latency_only_once() {
        let mut slot = None;
        let t0 = Instant::now();
        record_first_latency_ms_since(&mut slot, t0);
        let a = slot;
        record_first_latency_ms_since(&mut slot, t0);
        assert_eq!(slot, a);
    }

    #[test]
    fn capture_selection_report_once() {
        let mut slot = None;
        let mut bp = 0.0;
        let r1 = SelectionReport {
            tools_selected: vec!["a".into()],
            dynamic_tools_selected: vec!["a".into()],
            selected_count: 1,
            budget_used: 1,
            budget_total: 10,
        };
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r1.clone(), 0.3);
        assert_eq!(slot.as_ref().unwrap().tools_selected, vec!["a"]);
        assert!((bp - 0.3).abs() < f64::EPSILON);
        let r2 = SelectionReport {
            tools_selected: vec!["b".into()],
            dynamic_tools_selected: vec!["b".into()],
            selected_count: 1,
            budget_used: 2,
            budget_total: 10,
        };
        // Report stays first; pressure updates to peak
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r2, 0.9);
        assert_eq!(slot.as_ref().unwrap().tools_selected, vec!["a"]);
        assert!(
            (bp - 0.9).abs() < f64::EPSILON,
            "should track peak pressure"
        );
    }

    #[test]
    fn peak_pressure_does_not_regress() {
        let mut slot = None;
        let mut bp = 0.0;
        let r = SelectionReport {
            tools_selected: vec!["a".into()],
            dynamic_tools_selected: vec!["a".into()],
            selected_count: 1,
            budget_used: 1,
            budget_total: 10,
        };
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r.clone(), 0.8);
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r.clone(), 0.5);
        assert!((bp - 0.8).abs() < f64::EPSILON, "peak should not regress");
    }

    #[test]
    fn token_compact_under_1k() {
        assert_eq!(format_token_count_compact(0), "0tok");
        assert_eq!(format_token_count_compact(999), "999tok");
    }

    #[test]
    fn token_compact_thousands() {
        assert_eq!(format_token_count_compact(1000), "1.0k");
        assert_eq!(format_token_count_compact(12340), "12.3k");
    }

    #[test]
    fn session_abbrev() {
        assert_eq!(session_id_footer_abbrev(None), "?");
        assert_eq!(session_id_footer_abbrev(Some("")), "");
        assert_eq!(session_id_footer_abbrev(Some("abc")), "abc");
        assert_eq!(session_id_footer_abbrev(Some("1234567890")), "12345678");
        assert_eq!(
            session_id_footer_abbrev(Some("数据数据数据数据数据数据数据数据数据")),
            "数据数据数据数据"
        );
    }

    #[test]
    fn step_chat_run_id() {
        assert_eq!(
            step_recorder_chat_ephemeral_run_id(42),
            "chat-42".to_string()
        );
    }
}
