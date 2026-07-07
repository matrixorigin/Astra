use astra_turn_core::guardrails::explain_report_lines::{
    REPORT_SEPARATOR_LINE, VERDICT_REPORT_HEADER, verdict_avoid_tools_line,
    verdict_event_summary_line, verdict_injection_count_line, verdict_injection_preview_line,
    verdict_severity_icon,
};
use crossterm::style::Stylize;

use crate::VerdictEvent;
use crate::explain_dag::{ExplainTurnMeta, context_trace_from_json, render_explain_dag};

pub(super) fn render_explain_report_text(
    turns: &[serde_json::Value],
    meta: Option<&ExplainTurnMeta<'_>>,
    pending_context_assembly_trace: Option<&serde_json::Value>,
    verbose: bool,
) -> Option<String> {
    let trace = pending_context_assembly_trace.and_then(context_trace_from_json);
    render_explain_dag(trace.as_ref(), meta, turns, verbose)
}

pub(super) fn print_explain_report(
    turns: &[serde_json::Value],
    meta: Option<&ExplainTurnMeta<'_>>,
    pending_context_assembly_trace: Option<&serde_json::Value>,
    verbose: bool,
) {
    if let Some(text) =
        render_explain_report_text(turns, meta, pending_context_assembly_trace, verbose)
    {
        eprintln!("\n{text}");
        eprintln!("{}", REPORT_SEPARATOR_LINE.dim());
    }
}

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
                ve.suppressed_loop_nudges,
                ve.recent_error_pressure,
                ve.health_avoidance_count,
                ve.force_stop,
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

#[cfg(test)]
mod explain_preview_tests {
    use super::render_explain_report_text;
    use crate::explain_dag::ExplainTurnMeta;

    #[test]
    fn render_explain_report_text_uses_dag_and_cache_fields() {
        let tool_calls = vec![astra_services::session_journal::ToolCallRecord {
            tool_call_id: Some("call-1".into()),
            name: "git".into(),
            ok: true,
            ms: 12,
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(0),
            ..Default::default()
        }];
        let meta = ExplainTurnMeta {
            turn_label: Some("turn-2".into()),
            duration_ms: Some(1200),
            ttft_ms: Some(900),
            context_ms: Some(21),
            memoria_ms: Some(0),
            total_llm_ms: Some(1100),
            total_tool_ms: Some(12),
            prompt_tokens: Some(2211),
            completion_tokens: Some(525),
            cache_read_tokens: Some(19904),
            cache_creation_tokens: Some(0),
            tool_count: Some(1),
            llm_rounds: Some(1),
            routing_domain_hint: None,
            assistant_output: Some("done"),
            tool_call_records: &tool_calls,
            visible_tools: Vec::new(),
        };
        let turns = vec![serde_json::json!({
            "steps": [{
                "step": "llm",
                "duration_ms": 1100,
                "in": 2211,
                "cached_in": 19904,
                "cache_write": 0,
                "out": 525,
                "tool_calls": 1
            }]
        })];
        let text = render_explain_report_text(&turns, Some(&meta), None, false).expect("text");
        assert!(text.contains("Explain Analyze DAG — turn-2"));
        assert!(text.contains("cache_read=19904"));
        assert!(text.contains("batch[b-0-0] parallel tools=1"));
    }
}
