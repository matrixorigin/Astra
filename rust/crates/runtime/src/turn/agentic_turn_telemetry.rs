//! First-hit latency and selection-report capture for agentic `/chat` payload prep (CLI wires fields).

use std::time::Instant;

use crate::tool_registry::SelectionReport;

/// Set `*slot` to elapsed ms since `start` only when `slot` is still `None`.
pub fn record_first_latency_ms_since(slot: &mut Option<u64>, start: Instant) {
    if slot.is_none() {
        *slot = Some(start.elapsed().as_millis() as u64);
    }
}

/// On first selector pass, record latency and human-readable strategy line (matches historical CLI stderr).
pub fn record_first_selector_latency_and_strategy(
    first_ms: &mut Option<u64>,
    first_strategy: &mut Option<String>,
    sel_start: Instant,
    strategy: &str,
    confidence: f64,
) {
    if first_ms.is_none() {
        *first_ms = Some(sel_start.elapsed().as_millis() as u64);
        *first_strategy = Some(format!("{strategy} (conf={confidence:.2})"));
    }
}

/// Persist the first non-empty selection report and its budget pressure for cross-turn edge hints.
pub fn capture_first_selection_report_if_empty(
    slot: &mut Option<SelectionReport>,
    first_budget_pressure: &mut f64,
    report: SelectionReport,
    budget_pressure: f64,
) {
    if slot.is_none() {
        *slot = Some(report);
        *first_budget_pressure = budget_pressure;
    }
}

/// Add selector token accounting from one `select_with_learned_context` result.
pub fn accumulate_selector_token_usage(
    total_in: &mut u64,
    total_out: &mut u64,
    delta_in: u64,
    delta_out: u64,
) {
    *total_in += delta_in;
    *total_out += delta_out;
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
    fn first_selector_line_once() {
        let mut ms = None;
        let mut strat = None;
        let t0 = Instant::now();
        record_first_selector_latency_and_strategy(&mut ms, &mut strat, t0, "llm", 0.5);
        assert!(ms.is_some());
        assert_eq!(strat.as_deref(), Some("llm (conf=0.50)"));
        record_first_selector_latency_and_strategy(&mut ms, &mut strat, t0, "other", 0.9);
        assert_eq!(strat.as_deref(), Some("llm (conf=0.50)"));
    }

    #[test]
    fn accumulate_selector_tokens_adds() {
        let mut a = 0u64;
        let mut b = 0u64;
        accumulate_selector_token_usage(&mut a, &mut b, 3, 7);
        accumulate_selector_token_usage(&mut a, &mut b, 1, 2);
        assert_eq!(a, 4);
        assert_eq!(b, 9);
    }

    #[test]
    fn capture_selection_report_once() {
        let mut slot = None;
        let mut bp = 0.0;
        let r1 = SelectionReport {
            tools_selected: vec!["a".into()],
            selected_count: 1,
            budget_used: 1,
            budget_total: 10,
        };
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r1.clone(), 0.3);
        assert_eq!(slot.as_ref().unwrap().tools_selected, vec!["a"]);
        assert!((bp - 0.3).abs() < f64::EPSILON);
        let r2 = SelectionReport {
            tools_selected: vec!["b".into()],
            selected_count: 1,
            budget_used: 2,
            budget_total: 10,
        };
        capture_first_selection_report_if_empty(&mut slot, &mut bp, r2, 0.9);
        assert_eq!(slot.as_ref().unwrap().tools_selected, vec!["a"]);
        assert!((bp - 0.3).abs() < f64::EPSILON);
    }
}
