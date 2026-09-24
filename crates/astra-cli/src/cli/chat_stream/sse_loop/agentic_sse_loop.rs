//! Post-loop finalization: CLI sidecars (explain and verdict reports)
//! and [`StreamResult`] assembly from [`AgenticLoopState`].

use astra_core::canonical_names::normalize_name_list;
use astra_pipeline::{step_protocol::StepCheckpoint, step_recorder::StepRecorder};
use astra_runtime::turn::turn_guard::TurnGuard;
use astra_services::session_journal::ToolCallRecord;
use astra_turn_core::{
    tool_health_persistence::ToolHealthEntry, tool_registry_report::ToolSelectionReport,
};
use serde_json::Value;
use std::collections::HashSet;

use crate::cli::stream::streaming_types::{AppliedStreamUserIntent, UsageAttribution};
use crate::{ExplainMode, StreamResult, VerdictEvent};
use astra_config::runtime_config::ExplainReportFormat;

use crate::cli::chat_stream::verdict_reports::print_verdict_report;

pub(crate) struct StreamLoopSidecarEprint<'a> {
    pub(crate) explain: ExplainMode,
    pub(crate) explain_report_format: ExplainReportFormat,
    pub(crate) quiet: bool,
    pub(crate) explain_analyze_events: &'a [astra_turn_types::ExplainAnalyzeEventV1],
    pub(crate) explain_analyze_degraded: bool,
    pub(crate) verdict_events: &'a [VerdictEvent],
    pub(crate) current_session_id: Option<&'a str>,
}

pub(crate) fn eprint_stream_loop_sidecars(ctx: StreamLoopSidecarEprint<'_>) {
    let StreamLoopSidecarEprint {
        explain,
        explain_report_format,
        quiet,
        explain_analyze_events,
        explain_analyze_degraded,
        verdict_events,
        current_session_id,
    } = ctx;

    let mut explain_artifact_error = None;
    let explain_artifact = if explain != ExplainMode::Off {
        current_session_id.and_then(|session_id| {
            match crate::explain_analyze_artifact::persist_rendered_report(
                session_id,
                explain_analyze_events,
                explain_analyze_degraded,
                explain_report_format,
                explain == ExplainMode::Verbose,
            ) {
                Ok(publication) => publication,
                Err(error) => {
                    tracing::warn!("failed to persist Explain Analyze artifact: {error}");
                    explain_artifact_error = Some(error);
                    None
                }
            }
        })
    } else {
        None
    };

    if explain != ExplainMode::Off && !quiet {
        eprintln!(
            "{}",
            crate::explain_analyze_report::render(
                explain_analyze_events,
                explain == ExplainMode::Verbose,
                explain_analyze_degraded,
            )
        );
        if let Some(publication) = explain_artifact.as_ref() {
            eprintln!("{}", publication.terminal_notice());
        }
        if let Some(error) = explain_artifact_error.as_deref() {
            eprintln!("Explain Analyze artifact unavailable · {error}");
        }
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(verdict_events, explain == ExplainMode::Verbose);
    }
}

pub(crate) struct StreamResultBuild<'a> {
    pub(crate) qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    pub(crate) session_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) full_text: String,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) usage_attribution: UsageAttribution,
    pub(crate) tool_calls_count: u32,
    pub(crate) tool_ledger_aggregate:
        astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate,
    pub(crate) first_surface_report: Option<ToolSelectionReport>,
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
    pub(crate) memoria_ms: Option<u64>,
    pub(crate) routing_domain_hint: Option<String>,
    pub(crate) entity_learn_skipped_no_domain: bool,
    pub(crate) pending_context_assembly_trace: Option<(u32, serde_json::Value)>,
    pub(crate) turn_observability_events: Vec<astra_services::session_journal::JournalEvent>,
    pub(crate) llm_rounds: Option<u32>,
    pub(crate) token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage,
    pub(crate) interruption: Option<serde_json::Value>,
    pub(crate) server_terminal_unverified: bool,
    pub(crate) server_terminal_authoritative: bool,
    pub(crate) tool_record_coverage_partial: bool,
    pub(crate) final_messages: Vec<serde_json::Value>,
    pub(crate) deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
    pub(crate) run_transcript_messages: Vec<serde_json::Value>,
    pub(crate) applied_user_intents: Vec<AppliedStreamUserIntent>,
}

pub(crate) fn resolved_tool_metrics<I>(
    fallback_count: u32,
    fallback_tools: I,
    tool_call_records: &[ToolCallRecord],
) -> (u32, Vec<String>)
where
    I: IntoIterator<Item = String>,
{
    resolved_tool_metrics_with_authority(fallback_count, fallback_tools, tool_call_records, false)
}

fn resolved_tool_metrics_with_authority<I>(
    fallback_count: u32,
    fallback_tools: I,
    tool_call_records: &[ToolCallRecord],
    aggregate_authoritative: bool,
) -> (u32, Vec<String>)
where
    I: IntoIterator<Item = String>,
{
    let fallback_tools = fallback_tools.into_iter().collect::<Vec<_>>();
    if !aggregate_authoritative && tool_call_records.is_empty() {
        let mut tools_used = normalize_name_list(fallback_tools);
        tools_used.sort_unstable();
        return (fallback_count, tools_used);
    }

    // A Server-owned continuation exposes an authoritative aggregate while
    // the local ToolCallRecord ledger covers only the edge/request wrapper.
    // Keep both tool-name sources for audit, but never replace the aggregate
    // count with that partial record count.
    let mut tools_used = if aggregate_authoritative {
        fallback_tools
    } else {
        Vec::new()
    };
    let mut record_tool_calls_count = 0u32;
    for record in tool_call_records {
        if record.is_synthetic_placeholder() || record.was_blocked_by_policy() {
            continue;
        }
        let name = record.name.trim();
        if name.is_empty() {
            continue;
        }
        record_tool_calls_count += 1;
        tools_used.push(name.to_string());
    }

    let mut tools_used = normalize_name_list(tools_used);
    tools_used.sort_unstable();
    let tool_calls_count = if aggregate_authoritative || record_tool_calls_count == 0 {
        fallback_count
    } else {
        record_tool_calls_count
    };
    (tool_calls_count, tools_used)
}

fn non_empty_json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn interruption_visible_text(interruption: &Value, kind: Option<&str>) -> String {
    non_empty_json_str(interruption, "user_message")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let kind = kind.unwrap_or("interrupted");
            let suffix = if interruption
                .get("resumable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " You can continue in the next message."
            } else {
                ""
            };
            format!("[{kind}] Turn interrupted.{suffix}")
        })
}

pub(crate) fn partial_interruption_notice(result: &StreamResult) -> Option<String> {
    let interruption = result.interruption.as_ref()?;
    let notice = interruption_visible_text(interruption, result.interruption_kind.as_deref());
    let assistant_text = result.full_text.trim();
    // An interruption without provider text already uses this same safe
    // notice as its only visible text. Publish a separate lifecycle row only
    // when there is distinct partial assistant content to preserve.
    if assistant_text.is_empty() || assistant_text == notice.trim() {
        return None;
    }
    if result.interruption_kind.as_deref() == Some("execution_incomplete") {
        Some(format!("Partial answer shown. {notice}"))
    } else {
        Some(notice)
    }
}

pub(crate) fn build_stream_result(ctx: StreamResultBuild<'_>) -> StreamResult {
    let StreamResultBuild {
        qualified_usage,
        tool_health_entries,
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        usage_attribution,
        tool_calls_count,
        tool_ledger_aggregate,
        first_surface_report,
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
        memoria_ms,
        routing_domain_hint,
        entity_learn_skipped_no_domain,
        pending_context_assembly_trace,
        turn_observability_events,
        llm_rounds,
        token_usage_coverage,
        interruption,
        server_terminal_unverified,
        server_terminal_authoritative,
        tool_record_coverage_partial,
        final_messages,
        deferred_tool_activations,
        run_transcript_messages,
        applied_user_intents,
    } = ctx;
    let (_, tools_used) = resolved_tool_metrics_with_authority(
        tool_calls_count,
        tools_used,
        &tool_call_records,
        tool_record_coverage_partial,
    );
    let mut interruption = interruption;
    let mut server_terminal_unverified = server_terminal_unverified;
    if !tool_ledger_aggregate.is_complete_for(tool_calls_count) {
        server_terminal_unverified = true;
        if interruption.is_none() {
            interruption = Some(serde_json::json!({
                "kind": "execution_incomplete",
                "resume_action": "continue_immediately",
                "user_message": "Tool execution did not produce a complete canonical terminal aggregate.",
                "has_checkpoint": false,
                "tool_calls_completed": tool_ledger_aggregate.terminal,
                "turns_completed": 0,
                "remaining_turns": 0,
                "error_detail": "canonical tool result classes do not close against the logical tool-call count",
            }));
        }
    }
    let has_interruption = interruption.is_some();
    let interruption_kind = interruption
        .as_ref()
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut full_text = full_text;
    if full_text.trim().is_empty()
        && let Some(interruption) = interruption.as_ref()
    {
        full_text = interruption_visible_text(interruption, interruption_kind.as_deref());
    }
    let final_state = if has_interruption {
        "interrupted"
    } else if full_text.trim().is_empty() {
        "empty"
    } else {
        "completed"
    }
    .to_string();

    let report = first_surface_report.unwrap_or_else(|| ToolSelectionReport {
        visible_tools: Vec::new(),
        visible_count: 0,
        schema_budget_used: 0,
        schema_budget_total: 0,
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
        qualified_usage,
        session_id,
        run_id,
        session_persistence_error: None,
        full_text,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        usage_attribution,
        tool_calls_count,
        tool_ledger_aggregate,
        visible_tools: report.visible_tools,
        selected_skills,
        tools_used,
        tool_call_records,
        budget_used: report.schema_budget_used,
        budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms,
        context_ms,
        memoria_ms,
        routing_domain_hint,
        entity_learn_skipped_no_domain,
        pending_context_assembly_trace,
        turn_observability_events,
        llm_rounds,
        token_usage_coverage,
        interruption,
        final_state,
        interruption_kind,
        server_terminal_unverified,
        server_terminal_authoritative,
        tool_record_coverage_partial,
        final_messages,
        deferred_tool_activations,
        run_transcript_messages,
        applied_user_intents,
        background_agent_results: Vec::new(),
    }
}
#[cfg(test)]
mod tests {
    use super::{
        StreamResultBuild, build_stream_result, partial_interruption_notice, resolved_tool_metrics,
    };
    use crate::cli::stream::streaming_types::UsageAttribution;
    use astra_pipeline::step_recorder::StepRecorder;
    use astra_runtime::turn::turn_guard::TurnGuard;
    use astra_services::session_journal::ToolCallRecord;
    use std::collections::HashSet;

    use crate::VerdictEvent;

    fn make_step_recorder() -> StepRecorder {
        StepRecorder::with_persistence_for_run("test-user", "test-session", "test-task", "test-run")
    }

    fn make_turn_guard() -> TurnGuard {
        TurnGuard::new()
    }

    fn succeeded_aggregate(
        attempted: u32,
    ) -> astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate {
        astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate {
            attempted,
            terminal: attempted,
            unresolved: 0,
            result_classes: astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
                succeeded: attempted,
                ..Default::default()
            },
            consistent: true,
        }
    }

    fn make_build_ctx<'a>(
        step_recorder: &'a StepRecorder,
        turn_guard: &'a TurnGuard,
    ) -> StreamResultBuild<'a> {
        StreamResultBuild {
            qualified_usage: None,
            tool_health_entries: &[],
            session_id: Some("sess-1".into()),
            run_id: Some("run-1".into()),
            full_text: "hello".into(),
            prompt_tokens: 1000,
            completion_tokens: 500,
            cache_read_tokens: 800,
            cache_creation_tokens: 100,
            usage_attribution: UsageAttribution::default(),
            tool_calls_count: 3,
            tool_ledger_aggregate: succeeded_aggregate(3),
            first_surface_report: None,
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
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            token_usage_coverage: Default::default(),
            interruption: None,
            server_terminal_unverified: false,
            server_terminal_authoritative: false,
            tool_record_coverage_partial: false,
            final_messages: Vec::new(),
            deferred_tool_activations: vec![astra_turn_types::DeferredToolActivation {
                name: "memory".to_string(),
                schema_digest: "digest".to_string(),
                descriptor: None,
            }],
            run_transcript_messages: Vec::new(),
            applied_user_intents: Vec::new(),
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
        assert_eq!(result.final_state, "completed");
        assert_eq!(result.interruption_kind, None);
        assert_eq!(result.tool_calls_count, 3);
        assert_eq!(result.ttft_ms, Some(42));
        assert_eq!(result.context_ms, Some(100));
        assert_eq!(result.deferred_tool_activations.len(), 1);
    }

    #[test]
    fn build_stream_result_interrupts_when_canonical_classes_do_not_close() {
        let recorder = make_step_recorder();
        let guard = make_turn_guard();
        let mut ctx = make_build_ctx(&recorder, &guard);
        ctx.tool_ledger_aggregate.result_classes.succeeded = 2;

        let result = build_stream_result(ctx);

        assert_eq!(result.final_state, "interrupted");
        assert_eq!(
            result.interruption_kind.as_deref(),
            Some("execution_incomplete")
        );
        assert!(result.server_terminal_unverified);
    }

    #[test]
    fn build_stream_result_marks_interrupted_state() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.interruption = Some(serde_json::json!({"kind": "budget_exhausted"}));

        let result = build_stream_result(ctx);

        assert_eq!(result.final_state, "interrupted");
        assert_eq!(
            result.interruption_kind.as_deref(),
            Some("budget_exhausted")
        );
    }

    #[test]
    fn build_stream_result_surfaces_empty_text_interruption() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.full_text.clear();
        ctx.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "user_message": "[budget_exhausted] 46 tool call(s) completed. A checkpoint was saved. You can continue in the next message.",
            "error_detail": "The circuit breaker stopped this turn after round 14 because the model kept calling tools after the runtime injected a finalization correction."
        }));

        let result = build_stream_result(ctx);

        assert_eq!(result.final_state, "interrupted");
        assert_eq!(
            result.interruption_kind.as_deref(),
            Some("budget_exhausted")
        );
        assert!(result.full_text.contains("46 tool call(s) completed"));
        assert!(!result.full_text.contains("model kept calling tools"));
        assert_eq!(partial_interruption_notice(&result), None);
    }

    #[test]
    fn build_stream_result_keeps_partial_answer_and_interruption_notice_separate() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.full_text = "The requested file was updated successfully.".into();
        ctx.interruption = Some(serde_json::json!({
            "kind": "execution_incomplete",
            "resumable": true,
            "user_message": "Execution did not reach a verified terminal state. Progress is saved. Review the saved progress, then continue to reconcile the unfinished work.",
            "error_detail": "persistent unresolved tool outcome after bounded reconciliation"
        }));

        let result = build_stream_result(ctx);

        assert_eq!(
            result.full_text,
            "The requested file was updated successfully."
        );
        assert_eq!(
            partial_interruption_notice(&result).as_deref(),
            Some(
                "Partial answer shown. Execution did not reach a verified terminal state. Progress is saved. Review the saved progress, then continue to reconcile the unfinished work."
            )
        );
        assert!(!result.full_text.contains("persistent unresolved"));
    }

    #[test]
    fn build_stream_result_falls_back_when_interruption_lacks_user_message() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.full_text = "  ".into();
        ctx.interruption = Some(serde_json::json!({
            "kind": "harness_blocked",
            "resumable": false,
            "error_detail": "required harness capability is unavailable"
        }));

        let result = build_stream_result(ctx);

        assert_eq!(result.final_state, "interrupted");
        assert!(
            result
                .full_text
                .starts_with("[harness_blocked] Turn interrupted.")
        );
        assert!(!result.full_text.contains("required harness capability"));
    }

    #[test]
    fn build_stream_result_marks_malformed_interruption_as_interrupted() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.full_text.clear();
        ctx.interruption = Some(serde_json::json!({
            "resumable": true,
            "error_detail": "missing kind should not look completed"
        }));

        let result = build_stream_result(ctx);

        assert_eq!(result.final_state, "interrupted");
        assert_eq!(result.interruption_kind, None);
        assert!(
            result
                .full_text
                .starts_with("[interrupted] Turn interrupted.")
        );
        assert!(
            !result
                .full_text
                .contains("missing kind should not look completed")
        );
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
    fn resolved_tool_metrics_canonicalizes_fallback_tool_names() {
        let (count, tools) = resolved_tool_metrics(
            4,
            vec![
                " read_file".to_string(),
                " bash ".to_string(),
                "bash".to_string(),
                String::new(),
            ],
            &[],
        );

        assert_eq!(count, 4);
        assert_eq!(tools, vec!["bash".to_string(), "read_file".to_string()]);
    }

    #[test]
    fn resolved_tool_metrics_canonicalizes_record_tool_names() {
        let records = vec![
            tool_record(" bash ", true, Some("output")),
            tool_record("bash", true, Some("output")),
            tool_record(" ", true, Some("output")),
            tool_record(" read_file", true, Some("contents")),
        ];

        let (count, tools) = resolved_tool_metrics(0, std::iter::empty(), &records);

        assert_eq!(count, 3);
        assert_eq!(tools, vec!["bash".to_string(), "read_file".to_string()]);
    }

    #[test]
    fn build_stream_result_keeps_server_aggregate_over_partial_records() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.tool_calls_count = 34;
        ctx.tool_ledger_aggregate = succeeded_aggregate(34);
        ctx.tools_used = HashSet::from(["bash".to_string(), "write_file".to_string()]);
        ctx.server_terminal_authoritative = true;
        ctx.tool_record_coverage_partial = true;
        ctx.tool_call_records = vec![tool_record("bash", false, Some("first run failed"))];

        let result = build_stream_result(ctx);

        assert_eq!(result.tool_calls_count, 34);
        assert_eq!(
            result.tools_used,
            vec!["bash".to_string(), "write_file".to_string()]
        );
    }

    #[test]
    fn build_stream_result_keeps_remote_aggregate_after_edge_terminal() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.tool_calls_count = 34;
        ctx.tool_ledger_aggregate = succeeded_aggregate(34);
        ctx.tools_used = HashSet::from(["bash".to_string(), "write_file".to_string()]);
        ctx.tool_record_coverage_partial = true;
        ctx.tool_call_records = vec![tool_record("bash", false, Some("edge failure"))];

        let result = build_stream_result(ctx);

        assert_eq!(result.tool_calls_count, 34);
        assert!(!result.server_terminal_authoritative);
        assert!(result.tool_record_coverage_partial);
    }

    #[test]
    fn build_stream_result_excludes_blocked_tools_from_metrics() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.tool_calls_count = 2;
        ctx.tool_ledger_aggregate =
            astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate {
                attempted: 2,
                terminal: 2,
                unresolved: 0,
                result_classes: astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
                    succeeded: 1,
                    rejected: 1,
                    ..Default::default()
                },
                consistent: true,
            };
        ctx.tools_used = HashSet::from(["read_file".to_string(), "bash".to_string()]);
        ctx.tool_call_records = vec![
            // Blocked by restricted_tools policy — should be excluded
            ToolCallRecord {
                name: "read_file".into(),
                ok: false,
                result_class: Some(
                    astra_services::session_journal::BLOCKED_TOOL_RESULT_CLASS.into(),
                ),
                ..Default::default()
            },
            // Normal successful call — should be included
            tool_record("bash", true, Some("output")),
        ];

        let result = build_stream_result(ctx);

        // read_file was rejected, so it is absent from material tools_used but
        // remains one canonical attempted/result-class fact.
        assert_eq!(result.tools_used, vec!["bash".to_string()]);
        assert_eq!(result.tool_calls_count, 2);
    }

    #[test]
    fn build_stream_result_ignores_synthetic_tool_metrics() {
        let sr = make_step_recorder();
        let tg = make_turn_guard();
        let mut ctx = make_build_ctx(&sr, &tg);
        ctx.tool_calls_count = 1;
        ctx.tool_ledger_aggregate = succeeded_aggregate(1);
        ctx.tools_used = HashSet::from([
            "skill".to_string(),
            "bash".to_string(),
            "read_file".to_string(),
        ]);
        ctx.tool_call_records = vec![
            ToolCallRecord {
                name: "skill".into(),
                skill_reentry_count: Some(1),
                ..Default::default()
            },
            ToolCallRecord {
                name: astra_services::session_journal::SURGICAL_REMOVAL_TOOL_NAME.into(),
                surgically_removed: Some(true),
                original_tool_name: Some("bash".into()),
                ..Default::default()
            },
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
                health_avoidance_tools: vec![],
                advisory_threshold_reached: false,
                nudge_count: 0,
                interaction_mode: "prompt".into(),
                total_errors: 0,
                health_avoidance_count: 0,
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
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
                health_avoidance_tools: vec![],
                advisory_threshold_reached: false,
                nudge_count: 0,
                interaction_mode: "prompt".into(),
                total_errors: 0,
                health_avoidance_count: 0,
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
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
                health_avoidance_tools: vec![],
                advisory_threshold_reached: false,
                nudge_count: 0,
                interaction_mode: "prompt".into(),
                total_errors: 0,
                health_avoidance_count: 0,
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
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
