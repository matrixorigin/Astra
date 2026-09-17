use astra_turn_core::guardrails::explain_report_lines::{
    REPORT_SEPARATOR_LINE, VERDICT_REPORT_HEADER, verdict_avoid_tools_line,
    verdict_event_summary_line, verdict_injection_count_line, verdict_injection_preview_line,
    verdict_severity_icon,
};
use crossterm::style::Stylize;

use crate::VerdictEvent;
/// Print TurnGuard verdict details in explain mode.
pub(super) fn print_verdict_report(verdict_events: &[VerdictEvent], verbose: bool) {
    if verdict_events.is_empty() {
        return;
    }
    eprintln!("\n{}", VERDICT_REPORT_HEADER.dim());
    for ve in verdict_events {
        let icon = verdict_severity_icon(ve.severity.as_str());
        eprintln!(
            "{}",
            verdict_event_summary_line(
                ve.turn,
                icon,
                ve.severity.as_str(),
                ve.nudge_count,
                ve.interaction_mode.as_str(),
                ve.recent_error_pressure,
                ve.health_avoidance_count,
                ve.advisory_threshold_reached,
            )
            .dim()
        );
        if !ve.avoid_tools.is_empty() {
            eprintln!(
                "{}",
                verdict_avoid_tools_line(ve.avoid_tools.join(", ").as_str()).dim()
            );
        }
        if verbose {
            for (i, inj) in ve.injections.iter().enumerate() {
                let preview: String = inj.chars().take(120).collect();
                eprintln!(
                    "{}",
                    verdict_injection_preview_line(i, preview.as_str()).dim()
                );
            }
        } else if !ve.injections.is_empty() {
            eprintln!(
                "{}",
                verdict_injection_count_line(ve.injections.len()).dim()
            );
        }
    }
    eprintln!("{}", REPORT_SEPARATOR_LINE.dim());
}
