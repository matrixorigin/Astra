//! After the multi-turn loop: CLI sidecars (explain / verdict / verbose footer) and `StreamResult` assembly.

use std::collections::HashSet;
use std::time::Instant;

use crossterm::style::Stylize;
use mo_agent_runtime::{
    pipeline::persistence::ToolHealthEntry,
    pipeline::step_protocol::StepCheckpoint,
    pipeline::step_recorder::StepRecorder,
    tool_registry::{self},
    turn::turn_guard::TurnGuard,
};
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::{ExplainMode, StreamResult, VerdictEvent};

use super::super::explain_reports::{print_explain_report, print_verdict_report};

pub(crate) struct StreamLoopSidecarEprint<'a> {
    pub explain: ExplainMode,
    pub quiet: bool,
    pub verbose_mode: bool,
    pub start: Instant,
    pub model: Option<&'a str>,
    pub explain_turns: &'a [Value],
    pub verdict_events: &'a [VerdictEvent],
    pub has_any_usage: bool,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub current_session_id: Option<&'a str>,
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
    let format_footer_tokens = |tokens: u64| -> String {
        if tokens < 1000 {
            format!("{}tok", tokens)
        } else {
            format!("{:.1}k", tokens as f64 / 1000.0)
        }
    };
    let model_tag = model.unwrap_or("auto");
    let session_tag = current_session_id
        .map(|s| if s.len() > 8 { &s[..8] } else { s })
        .unwrap_or("?");
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_footer_tokens(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_footer_tokens(total_prompt)
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
    pub tool_health_entries: &'a [ToolHealthEntry],
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub full_text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_count: u32,
    pub first_selection_report: Option<tool_registry::SelectionReport>,
    pub selected_skills: Vec<String>,
    pub tools_used: HashSet<String>,
    pub tool_call_records: Vec<ToolCallRecord>,
    pub budget_pressure: f64,
    pub stall_events: Vec<(String, u32)>,
    pub verdict_events: Vec<VerdictEvent>,
    pub step_recorder: &'a StepRecorder,
    pub turn_guard: &'a TurnGuard,
    pub last_heavy_checkpoint: Option<StepCheckpoint>,
    pub ttft_ms: Option<u64>,
    pub context_ms: Option<u64>,
    pub selector_strategy: Option<String>,
    pub selector_ms: Option<u64>,
    pub selector_tokens_in: u64,
    pub selector_tokens_out: u64,
    pub memoria_ms: Option<u64>,
}

pub(crate) fn build_stream_result(ctx: StreamResultBuild<'_>) -> StreamResult {
    let StreamResultBuild {
        tool_health_entries,
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
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

    // Deduplicate stall events by type (keep only one of each type per user turn).
    // The internal _turn numbers were used for in-loop deduplication; for journal
    // output, we normalize all turn numbers to 0 (repl_turn.rs will use state.turn).
    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0)) // turn will be filled by repl_turn
            .collect()
    };

    // Deduplicate verdict events by severity (keep only the first of each severity).
    // Same rationale: internal turn numbers are loop-internal, not user turns.
    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0; // turn will be filled by repl_turn
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
