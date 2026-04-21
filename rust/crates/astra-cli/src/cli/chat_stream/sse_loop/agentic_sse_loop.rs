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
    pub(crate) selector_confidence: Option<f64>,
    pub(crate) selector_tokens_in: u64,
    pub(crate) selector_tokens_out: u64,
    pub(crate) memoria_ms: Option<u64>,
    pub(crate) routing_domain_hint: Option<String>,
    pub(crate) entity_learn_skipped_no_domain: bool,
    pub(crate) pending_context_assembly_trace: Option<(u32, serde_json::Value)>,
    pub(crate) turn_observability_events: Vec<astra_services::session_journal::JournalEvent>,
    pub(crate) llm_rounds: Option<u32>,
    pub(crate) prefetch_injected: bool,
    pub(crate) prefetch_task_type: Option<String>,
    pub(crate) prefetch_body_bytes: Option<usize>,
}

pub(crate) fn resolved_tool_metrics<I>(
    fallback_count: u32,
    fallback_tools: I,
    tool_call_records: &[ToolCallRecord],
) -> (u32, Vec<String>)
where
    I: IntoIterator<Item = String>,
{
    if tool_call_records.is_empty() {
        return (fallback_count, fallback_tools.into_iter().collect());
    }

    let mut seen = HashSet::new();
    let mut tools_used = Vec::new();
    let mut tool_calls_count = 0u32;
    for record in tool_call_records {
        if record.is_synthetic_placeholder() {
            continue;
        }
        tool_calls_count += 1;
        if seen.insert(record.name.clone()) {
            tools_used.push(record.name.clone());
        }
    }

    (tool_calls_count, tools_used)
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
        selector_confidence,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
        routing_domain_hint,
        entity_learn_skipped_no_domain,
        pending_context_assembly_trace,
        turn_observability_events,
        llm_rounds,
        prefetch_injected,
        prefetch_task_type,
        prefetch_body_bytes,
    } = ctx;
    let (tool_calls_count, tools_used) =
        resolved_tool_metrics(tool_calls_count, tools_used, &tool_call_records);

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
        tools_used,
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
        selector_confidence,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
        routing_domain_hint,
        entity_learn_skipped_no_domain,
        pending_context_assembly_trace,
        turn_observability_events,
        llm_rounds,
        prefetch_injected,
        prefetch_task_type,
        prefetch_body_bytes,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn make_step_recorder() -> StepRecorder {
        StepRecorder::with_persistence("test-session", "test-task")
    }

    fn make_turn_guard() -> TurnGuard {
        TurnGuard::new()
    }

    fn make_build_ctx<'a>(
        step_recorder: &'a StepRecorder,
        turn_guard: &'a TurnGuard,
    ) -> StreamResultBuild<'a> {
        StreamResultBuild {
            tool_health_entries: &[],
            session_id: Some("sess-1".into()),
            run_id: Some("run-1".into()),
            full_text: "hello".into(),
            prompt_tokens: 1000,
            completion_tokens: 500,
            cache_read_tokens: 800,
            cache_creation_tokens: 100,
            tool_calls_count: 3,
            first_selection_report: None,
            selected_skills: vec!["sk1".into()],
            tools_used: HashSet::from(["bash".into(), "read".into()]),
            tool_call_records: vec![],
            budget_pressure: 0.5,
            stall_events: vec![],
            verdict_events: vec![],
            step_recorder,
            turn_guard,
            last_heavy_checkpoint: None,
            ttft_ms: Some(42),
            context_ms: Some(100),
            selector_strategy: Some("tfidf".into()),
            selector_ms: Some(5),
            selector_confidence: Some(0.75),
            selector_tokens_in: 200,
            selector_tokens_out: 50,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            prefetch_injected: false,
            prefetch_task_type: None,
            prefetch_body_bytes: None,
        }
    }

    #[test]
    fn build_stream_result_passes_cache_tokens_through() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let ctx = make_build_ctx(&sr, &tg);
        let result = build_stream_result(ctx);
        assert_eq!(result.cache_read_tokens, 800);
        assert_eq!(result.cache_creation_tokens, 100);
        assert_eq!(result.prompt_tokens, 1000);
        assert_eq!(result.completion_tokens, 500);
    }

    #[test]
    fn build_stream_result_passes_basic_fields() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let ctx = make_build_ctx(&sr, &tg);
        let result = build_stream_result(ctx);
        assert_eq!(result.session_id.as_deref(), Some("sess-1"));
        assert_eq!(result.run_id.as_deref(), Some("run-1"));
        assert_eq!(result.full_text, "hello");
        assert_eq!(result.tool_calls_count, 3);
        assert_eq!(result.ttft_ms, Some(42));
        assert_eq!(result.context_ms, Some(100));
        assert_eq!(result.selector_strategy.as_deref(), Some("tfidf"));
    }

    fn tool_record(name: &str, ok: bool, result_preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: result_preview.map(str::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn build_stream_result_ignores_synthetic_tool_metrics() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.tool_calls_count = 5;
        ctx.tools_used = HashSet::from([
            "skill".to_string(),
            "bash".to_string(),
            "read_file".to_string(),
        ]);
        ctx.tool_call_records = vec![
            tool_record(
                "skill",
                false,
                Some(
                    "Skill 'debug' was already loaded (turn 2). Follow those instructions directly.",
                ),
            ),
            tool_record(
                "bash",
                false,
                Some("Skipped: the skill already completed this work. Do NOT call `bash` again."),
            ),
            tool_record("read_file", true, Some("contents")),
        ];

        let result = build_stream_result(ctx);

        assert_eq!(result.tool_calls_count, 1);
        assert_eq!(result.tools_used, vec!["read_file".to_string()]);
    }

    #[test]
    fn build_stream_result_deduplicates_stall_events() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.stall_events = vec![
            ("slow_tool".into(), 1),
            ("slow_tool".into(), 2),
            ("rate_limit".into(), 1),
        ];
        let result = build_stream_result(ctx);
        assert_eq!(result.stall_events.len(), 2);
        assert_eq!(result.stall_events[0].0, "slow_tool");
        assert_eq!(result.stall_events[1].0, "rate_limit");
    }

    #[test]
    fn build_stream_result_deduplicates_verdict_events() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.verdict_events = vec![
            VerdictEvent {
                severity: "warn".into(),
                turn: 1,
                injections: vec![],
                avoid_tools: vec![],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 0,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
            VerdictEvent {
                severity: "warn".into(),
                turn: 2,
                injections: vec![],
                avoid_tools: vec![],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 0,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
            VerdictEvent {
                severity: "error".into(),
                turn: 3,
                injections: vec![],
                avoid_tools: vec![],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 0,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        ];
        let result = build_stream_result(ctx);
        assert_eq!(result.verdict_events.len(), 2);
        assert_eq!(result.verdict_events[0].severity, "warn");
        assert_eq!(result.verdict_events[1].severity, "error");
        // Turns are reset to 0 in dedup
        assert_eq!(result.verdict_events[0].turn, 0);
    }

    #[test]
    fn build_stream_result_zero_cache_tokens() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.cache_read_tokens = 0;
        ctx.cache_creation_tokens = 0;
        let result = build_stream_result(ctx);
        assert_eq!(result.cache_read_tokens, 0);
        assert_eq!(result.cache_creation_tokens, 0);
    }
}
