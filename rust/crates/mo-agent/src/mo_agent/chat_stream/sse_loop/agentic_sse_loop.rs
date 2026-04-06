//! Post-loop finalization: CLI sidecars (explain, verdict, telemetry stderr)
//! and [`StreamResult`] assembly from [`AgenticLoopState`].

use std::collections::HashSet;
use std::time::Instant;

use astra_runtime::{
    pipeline::persistence::ToolHealthEntry,
    pipeline::step_protocol::StepCheckpoint,
    pipeline::step_recorder::StepRecorder,
    tool_registry,
    turn::agentic_turn_telemetry::{format_token_count_compact, session_id_footer_abbrev},
    turn::turn_guard::TurnGuard,
};
use astra_services::session_journal::ToolCallRecord;
use crossterm::style::Stylize;
use serde_json::Value;

use crate::{ExplainMode, StreamResult, VerdictEvent};

use super::super::explain_reports::{print_explain_report, print_verdict_report};

pub(crate) struct StreamLoopSidecarEprint<'a> {
    pub(crate) explain: ExplainMode,
    pub(crate) quiet: bool,
    pub(crate) verbose_mode: bool,
    pub(crate) start: Instant,
    pub(crate) model: Option<&'a str>,
    pub(crate) explain_turns: &'a [Value],
    pub(crate) verdict_events: &'a [VerdictEvent],
    pub(crate) has_any_usage: bool,
    pub(crate) total_prompt: u64,
    pub(crate) total_completion: u64,
    pub(crate) current_session_id: Option<&'a str>,
}

pub(crate) fn eprint_stream_loop_sidecars(ctx: StreamLoopSidecarEprint<'_>) {
    let StreamLoopSidecarEprint {
        explain,
        quiet,
        verbose_mode,
        start,
        model,
        explain_turns,
        verdict_events,
        has_any_usage,
        total_prompt,
        total_completion,
        current_session_id,
    } = ctx;

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(explain_turns, explain == ExplainMode::Verbose);
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(verdict_events, explain == ExplainMode::Verbose);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let model_tag = model.unwrap_or("auto");
    let session_tag = session_id_footer_abbrev(current_session_id);
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_token_count_compact(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_token_count_compact(total_prompt)
                } else {
                    "?".to_string()
                },
                model_tag,
                session_tag,
            )
            .dim()
        );
    }
}

pub(crate) struct StreamResultBuild<'a> {
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    pub(crate) session_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) full_text: String,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) tool_calls_count: u32,
    pub(crate) first_selection_report: Option<tool_registry::SelectionReport>,
    pub(crate) selected_skills: Vec<String>,
    pub(crate) tools_used: HashSet<String>,
    pub(crate) tool_call_records: Vec<ToolCallRecord>,
    pub(crate) budget_pressure: f64,
    pub(crate) stall_events: Vec<(String, u32)>,
    pub(crate) verdict_events: Vec<VerdictEvent>,
    pub(crate) step_recorder: &'a StepRecorder,
    pub(crate) turn_guard: &'a TurnGuard,
    pub(crate) last_heavy_checkpoint: Option<StepCheckpoint>,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) context_ms: Option<u64>,
    pub(crate) selector_strategy: Option<String>,
    pub(crate) selector_ms: Option<u64>,
    pub(crate) selector_tokens_in: u64,
    pub(crate) selector_tokens_out: u64,
    pub(crate) memoria_ms: Option<u64>,
}

pub(crate) fn build_stream_result(ctx: StreamResultBuild<'_>) -> StreamResult {
    let StreamResultBuild {
        tool_health_entries,
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        tool_calls_count,
        first_selection_report,
        selected_skills,
        tools_used,
        tool_call_records,
        budget_pressure,
        stall_events,
        verdict_events,
        step_recorder,
        turn_guard,
        last_heavy_checkpoint,
        ttft_ms,
        context_ms,
        selector_strategy,
        selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
    } = ctx;

    let report = first_selection_report.unwrap_or_else(|| tool_registry::SelectionReport {
        tools_selected: Vec::new(),
        selected_count: 0,
        budget_used: 0,
        budget_total: 0,
    });

    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0))
            .collect()
    };

    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0;
                ve
            })
            .collect()
    };

    StreamResult {
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        tool_calls_count,
        tools_selected: report.tools_selected,
        selected_skills,
        tools_used: tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms,
        context_ms,
        selector_strategy,
        selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
    }
}
