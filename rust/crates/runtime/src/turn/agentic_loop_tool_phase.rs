use std::collections::HashMap;

use serde_json::Value;

use astra_services::EvaluationService;
use astra_services::evaluation::SessionQualityAssessmentRequest;

use super::agentic_adaptive_tuning::{
    apply_per_turn_adaptation, apply_tactical_actions, maybe_run_tuning_cycle,
};
use super::agentic_auto_reflection::maybe_trigger_auto_reflection;
use super::agentic_delegate_interception::{
    DelegationInterceptionResult, intercept_delegations, tool_call_name,
};
use super::agentic_headless_round::{
    HeadlessRoundTerminal, HeadlessStderrStyle, HeadlessToolRoundCtx,
    run_agentic_headless_tool_round,
};
use super::agentic_loop_execution_phase::TurnExecutionPhase;
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CONSECUTIVE_ERROR_BUDGET,
    MAX_TRACKED_FILE_READS, extract_file_path_from_tool, finalize_turn_trace,
    record_edge_tool_observability,
};
use super::agentic_loop_lifecycle::TurnIterationPrep;
use super::agentic_post_tool_policy::{
    AgenticPostToolIterationControl, AgenticPostToolPolicyRequest, apply_agentic_post_tool_policy,
    map_post_tool_policy_outcome,
};
use super::agentic_tool_interception::{PreparedToolRound, prepare_intercepted_tool_round};
use super::agentic_turn_flow::{
    agentic_round_stall_preflight_with_tool_calls, append_explain_turn_batch,
};
use super::tool_result_semantics::tool_dedup_signature;
use crate::runtime_promotion_signals::{RuntimePromotionSignals, RuntimeTurnEvaluationSignal};

pub(crate) enum TurnToolPhaseControl {
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

fn build_runtime_session_quality_assessment(
    session_id: &str,
    quality: f64,
    total_tools: usize,
) -> SessionQualityAssessmentRequest {
    SessionQualityAssessmentRequest {
        session_id: session_id.to_string(),
        score: quality,
        step_count: i32::try_from(total_tools).unwrap_or(i32::MAX),
    }
}

async fn refresh_runtime_promotion_signals_from_db(state: &mut AgenticLoopState) {
    let (session_id, persistence) = match (
        state.current_session_id.as_deref(),
        state.telemetry.evaluation_persistence.clone(),
    ) {
        (Some(session_id), Some(persistence)) if !session_id.is_empty() => {
            (session_id.to_string(), persistence)
        }
        _ => return,
    };
    let verdict_warning =
        crate::server::run_lifecycle::has_turn_verdict_warning(&state.stall.verdict_events);
    let evaluation = crate::pipeline::evaluation::evaluate_tool_call_records(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
    );
    let assessment = build_runtime_session_quality_assessment(
        &session_id,
        evaluation.quality,
        state.step_recorder.summary().total_tools,
    );

    if let Err((status, response)) = persistence
        .evaluation_service
        .record_session_quality_assessment(&persistence.user_id, assessment)
        .await
    {
        astra_core::agent_warn!(
            "promotion-signals",
            "Failed to persist session quality assessment for {}: {} {}",
            session_id,
            status,
            response.0.detail
        );
        return;
    }

    match crate::server::run_lifecycle::load_runtime_promotion_signals_with_service(
        &persistence.evaluation_service,
        &persistence.user_id,
    )
    .await
    {
        Ok(signals) => {
            state.telemetry.runtime_promotion_signals = Some(signals.clone());
            if let Some(evolution_service) = state.evolution_service.as_ref() {
                evolution_service.set_runtime_promotion_signals(Some(signals));
            }
        }
        Err((status, response)) => astra_core::agent_warn!(
            "promotion-signals",
            "Failed to refresh runtime promotion signals for {}: {} {}",
            session_id,
            status,
            response.0.detail
        ),
    }
}

fn tool_record_result_text(rec: &astra_services::session_journal::ToolCallRecord) -> &str {
    rec.result_preview
        .as_deref()
        .or(rec.error.as_deref())
        .unwrap_or("")
}

fn tool_record_was_rejected(rec: &astra_services::session_journal::ToolCallRecord) -> bool {
    rec.error
        .as_deref()
        .map(|error| error.starts_with("blocked_tool:"))
        .unwrap_or(false)
}

fn turn_has_warning_verdict(
    turn_index: usize,
    verdict_events: &[super::agentic_verdict_audit::AgenticVerdictAuditEvent],
) -> bool {
    verdict_events.iter().any(|event| {
        event.turn == turn_index as u32
            && (event.severity.eq_ignore_ascii_case("warning")
                || event.severity.eq_ignore_ascii_case("critical"))
    })
}

fn refresh_runtime_promotion_signals_from_turn(
    existing: Option<&RuntimePromotionSignals>,
    input: &str,
    recent_tools: &[String],
    turn_index: usize,
    tool_call_records: &[astra_services::session_journal::ToolCallRecord],
    stall_events: &[(String, u32)],
    verdict_events: &[super::agentic_verdict_audit::AgenticVerdictAuditEvent],
    budget_pressure: f64,
) -> Option<RuntimePromotionSignals> {
    let stall_count = stall_events
        .iter()
        .filter(|(_, recorded_turn)| *recorded_turn == turn_index as u32)
        .count();
    let verdict_warning = turn_has_warning_verdict(turn_index, verdict_events);
    let evaluation = crate::pipeline::evaluation::evaluate_tool_call_records(
        input,
        recent_tools,
        tool_call_records,
        stall_count,
        verdict_warning,
        budget_pressure,
    );
    let recent_turn = (!tool_call_records.is_empty()
        || stall_count > 0
        || verdict_warning
        || evaluation.confidence >= 0.6)
        .then(|| {
            RuntimeTurnEvaluationSignal::from_turn_evaluation(
                &evaluation,
                tool_call_records.len(),
                stall_count,
                verdict_warning,
            )
        });

    RuntimePromotionSignals::with_recent_turn_feedback(existing, recent_turn)
}

const EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK: &str = "turn_rollback";

struct ServerRollbackBoundary {
    turn_index: u32,
    file_checkpoint: Option<u64>,
    database_checkpoint: Option<u64>,
    git_mutations: bool,
    session_state_checkpoint: Option<u64>,
}

fn server_file_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tool_call| {
        matches!(
            tool_call_name(tool_call),
            Some("write_file" | "str_replace" | "delete_file")
        )
    })
}

fn server_database_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls
        .iter()
        .any(|tool_call| matches!(tool_call_name(tool_call), Some("mo_query")))
}

fn server_git_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tool_call| {
        matches!(
            tool_call_name(tool_call),
            Some("git_commit" | "git_revert_commit")
        )
    })
}

fn server_session_state_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tool_call| {
        matches!(
            tool_call_name(tool_call),
            Some(
                "adjust_config"
                    | "prioritize_tool"
                    | "deprioritize_tool"
                    | "set_goal"
                    | "compress_context"
                    | "task_create"
                    | "task_update"
                    | "task_stop"
            )
        )
    })
}

fn append_session_journal_event(
    session_id: &str,
    event: astra_services::session_journal::JournalEvent,
) {
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(journal) => {
            if let Err(err) = journal.append(&event) {
                eprintln!("  ⚠ execution boundary journal append failed: {err}");
            }
        }
        Err(err) => eprintln!("  ⚠ execution boundary journal init failed: {err}"),
    }
}

fn server_boundary_surfaces(boundary: &ServerRollbackBoundary) -> Vec<&'static str> {
    let mut surfaces = Vec::new();
    if boundary.file_checkpoint.is_some() {
        surfaces.push("file_edits");
    }
    if boundary.database_checkpoint.is_some() {
        surfaces.push("database_snapshots");
    }
    if boundary.git_mutations {
        surfaces.push("git_mutations");
    }
    if boundary.session_state_checkpoint.is_some() {
        surfaces.push("session_state");
    }
    surfaces
}

fn server_boundary_checkpoints(boundary: &ServerRollbackBoundary) -> Value {
    let surfaces = server_boundary_surfaces(boundary);
    let mut checkpoints = serde_json::Map::from_iter([
        (
            "execution_mode".to_string(),
            Value::String("server".to_string()),
        ),
        (
            "rollback_surfaces".to_string(),
            Value::Array(
                surfaces
                    .iter()
                    .map(|surface| Value::String((*surface).to_string()))
                    .collect(),
            ),
        ),
    ]);
    if let Some(surface) = surfaces.first()
        && surfaces.len() == 1
    {
        checkpoints.insert(
            "rollback_surface".to_string(),
            Value::String((*surface).to_string()),
        );
    }
    if let Some(file_checkpoint) = boundary.file_checkpoint {
        checkpoints.insert(
            "file_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(file_checkpoint)),
        );
    }
    if let Some(database_checkpoint) = boundary.database_checkpoint {
        checkpoints.insert(
            "database_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(database_checkpoint)),
        );
    }
    if let Some(session_state_checkpoint) = boundary.session_state_checkpoint {
        checkpoints.insert(
            "session_state_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(session_state_checkpoint)),
        );
    }
    Value::Object(checkpoints)
}

fn server_boundary_commit_detail(
    boundary: &ServerRollbackBoundary,
    executed_requests: usize,
    file_entries_added: u64,
    database_entries_added: u64,
    git_mutations_recorded: u64,
    session_state_entries_added: u64,
) -> Value {
    let surfaces = server_boundary_surfaces(boundary);
    let mut detail = serde_json::Map::from_iter([
        (
            "executed_requests".to_string(),
            Value::Number(serde_json::Number::from(executed_requests as u64)),
        ),
        (
            "execution_mode".to_string(),
            Value::String("server".to_string()),
        ),
        (
            "rollback_surfaces".to_string(),
            Value::Array(
                surfaces
                    .iter()
                    .map(|surface| Value::String((*surface).to_string()))
                    .collect(),
            ),
        ),
    ]);
    if let Some(surface) = surfaces.first()
        && surfaces.len() == 1
    {
        detail.insert(
            "rollback_surface".to_string(),
            Value::String((*surface).to_string()),
        );
    }
    if boundary.file_checkpoint.is_some() {
        detail.insert(
            "file_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(file_entries_added)),
        );
    }
    if boundary.database_checkpoint.is_some() {
        detail.insert(
            "database_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(database_entries_added)),
        );
    }
    if boundary.git_mutations {
        detail.insert(
            "git_mutations_recorded".to_string(),
            Value::Number(serde_json::Number::from(git_mutations_recorded)),
        );
    }
    if boundary.session_state_checkpoint.is_some() {
        detail.insert(
            "session_state_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(session_state_entries_added)),
        );
    }
    Value::Object(detail)
}

fn parse_server_rollback_output(tool_name: &str, output: String) -> Value {
    serde_json::from_str(&output).unwrap_or_else(|error| {
        serde_json::json!({
            "success": false,
            "error": format!("invalid {tool_name} output: {error}"),
            "raw_output": output,
        })
    })
}

fn combine_server_rollback_outputs(
    turn_index: u32,
    file_edits: Option<Value>,
    database_snapshots: Option<Value>,
    git_mutations: Option<Value>,
    session_state: Option<Value>,
) -> Option<Value> {
    if file_edits.is_none()
        && database_snapshots.is_none()
        && git_mutations.is_none()
        && session_state.is_none()
    {
        return None;
    }

    let mut success = true;
    let mut summaries = Vec::new();
    let mut rollback = serde_json::Map::new();
    if let Some(file_edits) = file_edits {
        success &= file_edits
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = file_edits.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("file_edits".to_string(), file_edits);
    }
    if let Some(database_snapshots) = database_snapshots {
        success &= database_snapshots
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = database_snapshots.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("database_snapshots".to_string(), database_snapshots);
    }
    if let Some(git_mutations) = git_mutations {
        success &= git_mutations
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = git_mutations.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("git_mutations".to_string(), git_mutations);
    }
    if let Some(session_state) = session_state {
        success &= session_state
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = session_state.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("session_state".to_string(), session_state);
    }
    rollback.insert("success".to_string(), Value::Bool(success));
    rollback.insert(
        "summary".to_string(),
        Value::String(if summaries.is_empty() {
            format!("Attempted bounded rollback for turn {turn_index}")
        } else {
            summaries.join(" ")
        }),
    );
    Some(Value::Object(rollback))
}

fn server_git_mutation_targets(tool_results: &[Value]) -> Vec<String> {
    tool_results
        .iter()
        .filter_map(|tool_result| {
            let tool_name = tool_result.get("name").and_then(Value::as_str)?;
            match tool_name {
                "git_commit" => tool_result.get("commit_sha").and_then(Value::as_str),
                "git_revert_commit" => tool_result.get("revert_commit_sha").and_then(Value::as_str),
                _ => None,
            }
            .map(ToString::to_string)
        })
        .collect()
}

async fn rollback_server_git_mutations(
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    targets: &[String],
) -> Option<Value> {
    if targets.is_empty() {
        return None;
    }

    let mut reverted = Vec::new();
    let mut failed = Vec::new();
    for commit_sha in targets.iter().rev() {
        let result = executor
            .execute_with_metadata(
                "git_revert_commit",
                &serde_json::json!({ "commit_sha": commit_sha }),
            )
            .await;
        let mut entry = serde_json::Map::from_iter([(
            "commit_sha".to_string(),
            Value::String(commit_sha.clone()),
        )]);
        if let Some(metadata) = result.metadata {
            entry.extend(metadata);
        }
        if result.is_error {
            entry.insert("error".to_string(), Value::String(result.output));
            failed.push(Value::Object(entry));
        } else {
            entry.insert("result".to_string(), Value::String(result.output));
            reverted.push(Value::Object(entry));
        }
    }

    let success = !reverted.is_empty() && failed.is_empty();
    let summary = if failed.is_empty() {
        format!(
            "Created {} compensating git revert commit{} during turn rollback",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Created {} compensating git revert commit{} during turn rollback with {} failure{}",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" },
            failed.len(),
            if failed.len() == 1 { "" } else { "s" }
        )
    };

    Some(serde_json::json!({
        "success": success,
        "reverted": reverted,
        "failed": failed,
        "summary": summary,
    }))
}

fn open_server_rollback_boundary(
    session_id: Option<&str>,
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    turn_index: u32,
    tool_calls: &[Value],
) -> Option<ServerRollbackBoundary> {
    let has_file_mutator = server_file_mutator_in_round(tool_calls);
    let has_database_mutator = server_database_mutator_in_round(tool_calls);
    let has_git_mutator = server_git_mutator_in_round(tool_calls);
    let has_session_state_mutator = server_session_state_mutator_in_round(tool_calls);
    if !has_file_mutator && !has_database_mutator && !has_git_mutator && !has_session_state_mutator
    {
        return None;
    }

    let active = ServerRollbackBoundary {
        turn_index,
        file_checkpoint: has_file_mutator.then(|| executor.file_journal_checkpoint()),
        database_checkpoint: has_database_mutator
            .then(|| executor.database_snapshot_journal_checkpoint()),
        git_mutations: has_git_mutator,
        session_state_checkpoint: has_session_state_mutator
            .then(|| executor.session_state_journal_checkpoint()),
    };
    if let Some(session_id) = session_id {
        append_session_journal_event(
            session_id,
            astra_services::session_journal::JournalEvent::execution_boundary_opened(
                Some(session_id),
                turn_index,
                EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                None,
                server_boundary_checkpoints(&active),
            ),
        );
    }
    Some(active)
}

async fn finalize_server_rollback_boundary(
    session_id: Option<&str>,
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    active: &ServerRollbackBoundary,
    new_records: &[astra_services::session_journal::ToolCallRecord],
    new_tool_results: &[Value],
) {
    let file_entries_added = active.file_checkpoint.map_or(0, |checkpoint| {
        executor
            .file_journal_checkpoint()
            .saturating_sub(checkpoint)
    });
    let database_entries_added = active.database_checkpoint.map_or(0, |checkpoint| {
        executor
            .database_snapshot_journal_checkpoint()
            .saturating_sub(checkpoint)
    });
    let session_state_entries_added = active.session_state_checkpoint.map_or(0, |checkpoint| {
        executor
            .session_state_journal_checkpoint()
            .saturating_sub(checkpoint)
    });
    let git_mutation_targets = if active.git_mutations {
        server_git_mutation_targets(new_tool_results)
    } else {
        Vec::new()
    };
    let git_mutations_recorded = git_mutation_targets.len() as u64;
    if let Some(failed_record) = new_records.iter().find(|record| !record.ok) {
        let file_rollback = if let Some(file_checkpoint) = active.file_checkpoint {
            (file_entries_added > 0).then(|| {
                parse_server_rollback_output(
                    "rollback_file_edits",
                    executor.rollback_file_edits(&serde_json::json!({
                        "scope": "current_turn",
                        "after_sequence": file_checkpoint,
                    })),
                )
            })
        } else {
            None
        };
        let database_snapshot_rollback =
            if let Some(database_checkpoint) = active.database_checkpoint {
                (database_entries_added > 0).then(|| {
                    parse_server_rollback_output(
                        "rollback_database_snapshots",
                        executor.rollback_database_snapshots(&serde_json::json!({
                            "scope": "current_turn",
                            "database_after_sequence": database_checkpoint,
                        })),
                    )
                })
            } else {
                None
            };
        let git_mutation_rollback =
            rollback_server_git_mutations(executor, &git_mutation_targets).await;
        let session_state_rollback =
            if let Some(session_state_checkpoint) = active.session_state_checkpoint {
                (session_state_entries_added > 0).then(|| {
                    parse_server_rollback_output(
                        "rollback_session_state",
                        executor.rollback_session_state(&serde_json::json!({
                            "scope": "current_turn",
                            "session_state_after_sequence": session_state_checkpoint,
                        })),
                    )
                })
            } else {
                None
            };
        let rollback = combine_server_rollback_outputs(
            active.turn_index,
            file_rollback,
            database_snapshot_rollback,
            git_mutation_rollback,
            session_state_rollback,
        );
        if let Some(session_id) = session_id {
            append_session_journal_event(
                session_id,
                astra_services::session_journal::JournalEvent::execution_boundary_aborted(
                    Some(session_id),
                    active.turn_index,
                    EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                    None,
                    "tool_error",
                    Some(failed_record.name.as_str()),
                    None,
                    rollback,
                ),
            );
        }
    } else if let Some(session_id) = session_id {
        append_session_journal_event(
            session_id,
            astra_services::session_journal::JournalEvent::execution_boundary_committed(
                Some(session_id),
                active.turn_index,
                EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                None,
                Some(server_boundary_commit_detail(
                    active,
                    new_records.len(),
                    file_entries_added,
                    database_entries_added,
                    git_mutations_recorded,
                    session_state_entries_added,
                )),
            ),
        );
    }
}

pub(crate) async fn execute_tool_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
    phase: TurnExecutionPhase,
) -> Result<TurnToolPhaseControl, String> {
    let TurnExecutionPhase {
        llm_wall_start,
        turn_result,
    } = phase;

    let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
        turn_index,
        &turn_result.accum.tool_calls,
        &turn_result.edge_tool_round,
        &mut state.stall.turn_sigs,
        &mut state.stall.turn_tool_names,
        &mut state.stall.events,
        &mut state.turn_guard,
    );

    let DelegationInterceptionResult {
        effective_tool_calls,
        intercepted_any: delegation_intercepted,
    } = intercept_delegations(host, state, &turn_result, prep.quiet).await;

    let PreparedToolRound {
        tool_calls,
        pre_resolved_results,
        edge_tool_round,
    } = prepare_intercepted_tool_round(
        state,
        &turn_result,
        &effective_tool_calls,
        delegation_intercepted,
    )
    .await;
    let all_tool_calls = tool_calls.as_slice();
    let edge_round_for_headless = edge_tool_round.as_slice();
    let active_server_rollback_boundary =
        state.server_tool_executor.as_deref().and_then(|executor| {
            open_server_rollback_boundary(
                state.current_session_id.as_deref(),
                executor,
                turn_index as u32,
                all_tool_calls,
            )
        });

    let errors_before_round = state.turn_guard.errors.total_errors;
    let errors_by_cat_before = state.turn_guard.errors.errors_by_category.clone();

    struct HostTerminalAdapter<'a, H: AgenticLoopHost>(&'a mut H);
    impl<H: AgenticLoopHost> HeadlessRoundTerminal for HostTerminalAdapter<'_, H> {
        fn emit_line(&mut self, style: HeadlessStderrStyle, line: String) {
            self.0.emit_headless_line(style, line);
        }
    }

    let edge_callback_outputs: HashMap<String, String> = turn_result
        .edge_tool_round
        .iter()
        .map(|r| (tool_dedup_signature(&r.tool, &r.args), r.output.clone()))
        .collect();

    let evo_records_before = state.stall.tool_call_records.len();
    let tool_results_before = state.tool_results.len();
    {
        let valid_tool_names = host.valid_tool_names().clone();
        let mut term_adapter = HostTerminalAdapter(host);
        let headless_quiet = prep.quiet || state.skill_produced_output;
        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index,
            quiet: headless_quiet,
            api: &state.api,
            token: &state.api_token,
            current_session_id: state.current_session_id.as_ref(),
            tool_calls: all_tool_calls,
            edge_tool_round: edge_round_for_headless,
            reasoning_content: turn_result.accum.reasoning_content.as_str(),
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut state.messages,
            tool_results: &mut state.tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut state.restricted_tools,
            turn_guard: &mut state.turn_guard,
            step_recorder: &mut state.step_recorder,
            idempotency_cache: &mut state.idempotency_cache,
            semantic_dedup: &mut state.semantic_dedup,
            call_counts: &mut state.call_counts,
            max_identical_calls: state.max_identical_tool_calls,
            max_tools_per_turn: state.max_tools_per_turn,
            tool_call_records: &mut state.stall.tool_call_records,
            tool_event_hooks: &state.skills.tool_event_hooks,
            term: &mut term_adapter,
            mailbox: state.messaging.mailbox.as_mut(),
            permission_context: state.permission_context.as_ref(),
            progress_emitter: state.messaging.progress_emitter.as_ref(),
            pre_resolved_results: &pre_resolved_results,
            server_tool_executor: state.server_tool_executor.as_deref(),
        })
        .await;
    }
    if let (Some(active), Some(executor)) = (
        active_server_rollback_boundary.as_ref(),
        state.server_tool_executor.as_deref(),
    ) {
        let new_records = &state.stall.tool_call_records[evo_records_before..];
        let new_tool_results = &state.tool_results[tool_results_before..];
        finalize_server_rollback_boundary(
            state.current_session_id.as_deref(),
            executor,
            active,
            new_records,
            new_tool_results,
        )
        .await;
    }

    if let Some(ref evo) = state.evolution_service {
        let turn_id = state.current_run_id.as_deref().unwrap_or("unknown");
        let active_skill: Option<String> = state
            .skills
            .invoked
            .iter()
            .max_by_key(|(_, v)| v.invoked_at_turn)
            .map(|(name, _)| name.clone());
        let active_skill_ref = active_skill.as_deref();
        for rec in &state.stall.tool_call_records[evo_records_before..] {
            if rec.is_synthetic_placeholder() {
                continue;
            }
            let result_text = tool_record_result_text(rec);
            let classification = crate::turn::action_compensation::classify_execution_outcome(
                result_text,
                !rec.ok,
                rec.ms,
                tool_record_was_rejected(rec),
            );
            let ctx = crate::evolution::types::ToolResultContext {
                tool_name: &rec.name,
                tool_args: rec.args_preview.as_deref().unwrap_or(""),
                result: result_text,
                is_error: !rec.ok,
                failure_category: classification.failure_category,
                duration_ms: rec.ms,
                active_skill: active_skill_ref,
                turn_id,
            };
            evo.on_tool_result(&ctx).await;
        }

        if !state.stall.turn_sigs.is_empty() {
            let sigs = &state.stall.turn_sigs;
            let n = sigs.len();
            if n >= 3 && sigs[n - 1] == sigs[n - 2] && sigs[n - 2] == sigs[n - 3] {
                let chain: Vec<String> = sigs[n - 1].iter().cloned().collect();
                evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: chain,
                    stall_count: 3,
                    turn_id: turn_id.to_string(),
                })
                .await;
            }
        }

        let this_turn = &state.stall.tool_call_records[evo_records_before..];
        let mut fail_counts: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();
        for rec in this_turn {
            if !rec.ok {
                *fail_counts.entry(rec.name.as_str()).or_default() += 1;
            }
        }
        for (tool, count) in &fail_counts {
            if *count >= 3 {
                evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![(*tool).to_string()],
                    stall_count: *count,
                    turn_id: turn_id.to_string(),
                })
                .await;
            }
        }
    }

    if state.step_signal_collector.is_some() || state.tactical_adapter.is_some() {
        let new_records = &state.stall.tool_call_records[evo_records_before..];
        let mut step_actions: Vec<crate::liquid::tactical::TacticalAction> = Vec::new();

        for rec in new_records {
            let outcome = crate::liquid::step_signals::StepOutcome {
                tool_name: rec.name.clone(),
                ok: rec.ok,
                latency_ms: rec.ms,
                tokens_used: (rec.input_bytes.unwrap_or(0) + rec.output_bytes.unwrap_or(0)) as u64,
                error_hint: rec.error.clone(),
            };
            let triggers = if let Some(ref mut collector) = state.step_signal_collector {
                collector.record(outcome)
            } else {
                vec![]
            };
            if !triggers.is_empty()
                && let Some(ref mut adapter) = state.tactical_adapter
            {
                let actions = adapter.evaluate(&triggers);
                for action in actions {
                    if !matches!(action, crate::liquid::tactical::TacticalAction::NoOp) {
                        step_actions.push(action);
                    }
                }
                adapter.advance_step();
            }
        }

        if !step_actions.is_empty() {
            let hint_parts = apply_tactical_actions(state, &step_actions);
            if !hint_parts.is_empty() {
                let hint_text = format!("[Tactical Adaptation]\n{}", hint_parts.join("\n"));
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": hint_text
                }));
            }
        }
    }

    if let Some(ref emitter) = state.messaging.progress_emitter {
        for rec in &state.stall.tool_call_records {
            if let Some(ref err) = rec.error
                && err.starts_with("blocked_tool:")
            {
                emitter.permission_denied(
                    &rec.name,
                    err.trim_start_matches("blocked_tool: "),
                    turn_index as u32,
                );
            }
        }
    }

    append_explain_turn_batch(
        &mut state.telemetry.explain_turns,
        turn_result.accum.explain_turns.as_slice(),
    );

    {
        let turn_num = (state.max_turns - state.remaining_turns) as u32;
        for edge_result in &turn_result.edge_tool_round {
            if let Some(path) = extract_file_path_from_tool(&edge_result.tool, &edge_result.args) {
                if let Some(existing) = state.recent_file_reads.iter_mut().find(|(p, _)| p == &path)
                {
                    existing.1 = turn_num;
                } else {
                    state.recent_file_reads.push((path, turn_num));
                }
                if state.recent_file_reads.len() > MAX_TRACKED_FILE_READS {
                    state.recent_file_reads.sort_by_key(|(_, t)| *t);
                    state.recent_file_reads.remove(0);
                }
            }
        }
    }

    record_edge_tool_observability(state, &turn_result.edge_tool_round);

    if let Some(ref registry) = state.skills.registry_for_activation {
        let mut any_newly_activated = false;
        for edge_result in &turn_result.edge_tool_round {
            if let Some(path) = extract_file_path_from_tool(&edge_result.tool, &edge_result.args) {
                let newly = registry.record_file_path(&path);
                if !newly.is_empty() {
                    any_newly_activated = true;
                    if !prep.quiet {
                        for name in &newly {
                            host.emit_headless_line(
                                HeadlessStderrStyle::Dim,
                                format!("  ◆ Skill activated: {name}"),
                            );
                        }
                    }
                }
            }
        }
        if any_newly_activated && let Some(resolver) = &state.skills.resolver {
            let full = resolver.available_skills();
            if !full.is_empty() {
                let (visible, open_skill_name) =
                    crate::turn::skill_tool::visible_skills_for_host_turn(
                        &full,
                        state.message.as_str(),
                        &state.skills.quality_tracker,
                        &state.skills.pinned,
                        &state.skills.discovered,
                        &state.skills.search,
                    );
                host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(
                    &visible,
                    Some(&state.skills.quality_tracker),
                    Some(&state.skills.pinned),
                    open_skill_name,
                ));
                if open_skill_name {
                    host.inject_tool_schema(crate::turn::skill_tool::discover_skills_tool_schema());
                }
            }
        }
    }

    {
        let turn_errors = state
            .turn_guard
            .errors
            .total_errors
            .saturating_sub(errors_before_round);
        if turn_errors > 0 {
            let dominant = state
                .turn_guard
                .errors
                .errors_by_category
                .iter()
                .filter_map(|(cat, &count)| {
                    let before = errors_by_cat_before.get(cat).copied().unwrap_or(0);
                    let delta = count.saturating_sub(before);
                    if delta > 0 { Some((*cat, delta)) } else { None }
                })
                .max_by_key(|(_, delta)| *delta)
                .map(|(cat, _)| cat);
            if dominant == state.error_recovery.last_error_category {
                state.error_recovery.consecutive_same_error += 1;
            } else {
                state.error_recovery.consecutive_same_error = 1;
                state.error_recovery.last_error_category = dominant;
            }
            if state.error_recovery.consecutive_same_error >= CONSECUTIVE_ERROR_BUDGET {
                let cat_name = state
                    .error_recovery
                    .last_error_category
                    .map(|c| format!("{c:?}"))
                    .unwrap_or_else(|| "Unknown".into());
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "🔄 ERROR BUDGET EXHAUSTED: You've hit {cat_name} errors \
                         {n} turns in a row. Your current approach is not working. \
                         STOP repeating the same strategy. You MUST try a fundamentally \
                         different approach: different tool, different file, different \
                         method. If you cannot make progress, explain what's blocking you.",
                        n = state.error_recovery.consecutive_same_error,
                    )
                }));
                state.error_recovery.consecutive_same_error = 0;
            }
        } else {
            state.error_recovery.consecutive_same_error = 0;
            state.error_recovery.last_error_category = None;
        }
    }

    if let Some(ref gate) = state.checkpoint_gate {
        let freq = gate.checkpoint_frequency();
        if freq > 0 && (turn_index as u32 + 1).is_multiple_of(freq) {
            let run_id = state.current_run_id.as_deref().unwrap_or("unknown");
            match gate
                .check(run_id, turn_index as u32, state.total_tool_calls)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    let updated_promotion_signals = refresh_runtime_promotion_signals_from_turn(
                        state.telemetry.runtime_promotion_signals.as_ref(),
                        &state.message,
                        &state.recent_tools,
                        turn_index,
                        &state.stall.tool_call_records[evo_records_before..],
                        &state.stall.events,
                        &state.stall.verdict_events,
                        state.telemetry.first_budget_pressure,
                    );
                    state.telemetry.runtime_promotion_signals = updated_promotion_signals;
                    observe_gate_cancelled(state, turn_index, prep.turn_start_time, &turn_result);
                    state.step_recorder.end_turn(true);
                    finalize_turn_trace(state).await;
                    refresh_runtime_promotion_signals_from_db(state).await;
                    return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Cancelled));
                }
                Err(e) => {
                    eprintln!("[checkpoint-gate] check error: {e}");
                }
            }
        }
    }

    match map_post_tool_policy_outcome(apply_agentic_post_tool_policy(
        AgenticPostToolPolicyRequest {
            turn_index: turn_index as u32,
            message: &state.message,
            tool_calls_for_guard: &tool_calls_for_guard,
            intent_tool_turns: &mut state.stall.intent_tool_turns,
            messages: &mut state.messages,
            stall_events: &mut state.stall.events,
            turn_guard: &mut state.turn_guard,
            verdict_events: &mut state.stall.verdict_events,
            restricted_tools: &mut state.restricted_tools,
            remaining_turns: &mut state.remaining_turns,
            step_recorder: &mut state.step_recorder,
            current_session_id: state.current_session_id.as_ref(),
            max_turns: state.max_turns,
            loop_turn: turn_index,
            recent_tools: &state.recent_tools,
            last_heavy_checkpoint: &mut state.stall.last_heavy_checkpoint,
        },
    )) {
        AgenticPostToolIterationControl::Abort(e) => {
            let updated_promotion_signals = refresh_runtime_promotion_signals_from_turn(
                state.telemetry.runtime_promotion_signals.as_ref(),
                &state.message,
                &state.recent_tools,
                turn_index,
                &state.stall.tool_call_records[evo_records_before..],
                &state.stall.events,
                &state.stall.verdict_events,
                state.telemetry.first_budget_pressure,
            );
            state.telemetry.runtime_promotion_signals = updated_promotion_signals;
            state.step_recorder.end_turn(true);
            finalize_turn_trace(state).await;
            refresh_runtime_promotion_signals_from_db(state).await;
            return Err(e);
        }
        AgenticPostToolIterationControl::RetryLlmClearToolResults => {
            state.tool_results.clear();
        }
        AgenticPostToolIterationControl::ProceedEndTurn => {
            if let Some(ref emitter) = state.messaging.progress_emitter {
                let tool_calls_this_turn =
                    state.total_tool_calls.saturating_sub(if turn_index > 0 {
                        state.total_tool_calls
                    } else {
                        0
                    });
                let last_tool = turn_result
                    .edge_tool_round
                    .last()
                    .map(|r| r.tool.clone())
                    .unwrap_or_else(|| "thinking".to_string());
                emitter.turn_completed(turn_index as u32 + 1, tool_calls_this_turn, last_tool);
                emitter.metrics_update(
                    turn_index as u32 + 1,
                    state.max_turns as u32,
                    state.total_prompt,
                    state.total_completion,
                    state.total_tool_calls,
                );
            }

            if let Some(ref mailbox) = state.messaging.mailbox
                && mailbox.has_parent().await
            {
                if let Err(e) = mailbox
                    .send_progress(
                        turn_index as u32,
                        state.total_tool_calls,
                        "turn_complete",
                        None,
                    )
                    .await
                {
                    astra_core::agent_warn!("mailbox", "Failed to send turn progress: {e}");
                }
            }

            if let (Some(hub), Some(session)) = (
                state.telemetry.observability_hub.as_ref(),
                state.telemetry.observability_session.as_ref(),
            ) {
                let total_ms = prep.turn_start_time.elapsed().as_millis() as u64;
                let ctx_asm_ms = (llm_wall_start - prep.turn_start_time).as_millis() as u64;
                let tool_exec_ms: u64 = turn_result
                    .edge_tool_round
                    .iter()
                    .map(|e| e.duration_ms)
                    .sum();
                let timing = crate::observability_integration::TurnTiming {
                    turn: turn_index as u32,
                    context_assembly_ms: ctx_asm_ms,
                    ttft_ms: turn_result.ttft_ms.unwrap_or(0),
                    llm_total_ms: total_ms
                        .saturating_sub(ctx_asm_ms)
                        .saturating_sub(tool_exec_ms),
                    tool_execution_ms: tool_exec_ms,
                    total_ms,
                };
                let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
            }

            let updated_promotion_signals = refresh_runtime_promotion_signals_from_turn(
                state.telemetry.runtime_promotion_signals.as_ref(),
                &state.message,
                &state.recent_tools,
                turn_index,
                &state.stall.tool_call_records[evo_records_before..],
                &state.stall.events,
                &state.stall.verdict_events,
                state.telemetry.first_budget_pressure,
            );
            state.telemetry.runtime_promotion_signals = updated_promotion_signals;
            state.step_recorder.end_turn(false);
            finalize_turn_trace(state).await;
            refresh_runtime_promotion_signals_from_db(state).await;
            state.telemetry.completed_turns_for_tuning += 1;
            maybe_run_tuning_cycle(state);
            maybe_trigger_auto_reflection(host, state).await;
            let turn_tokens = state.last_measured_prompt_tokens.unwrap_or(0);
            apply_per_turn_adaptation(state, turn_tokens);
        }
    }

    Ok(TurnToolPhaseControl::ContinueLoop)
}

fn observe_gate_cancelled(
    state: &mut AgenticLoopState,
    turn_index: usize,
    turn_start_time: std::time::Instant,
    turn_result: &super::agentic_loop_host::HostTurnResult,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability_integration::TurnTiming {
            turn: turn_index as u32,
            context_assembly_ms: 0,
            ttft_ms: turn_result.ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::confidence::ConfidenceInterval;
    use astra_services::session_journal::{
        JournalEvent, JournalEventType, JournalWriter, ToolCallRecord,
    };
    use serde_json::json;

    fn summary_tool_record(
        ok: bool,
        error: Option<&str>,
        result_preview: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok,
            ms: 100,
            error: error.map(str::to_string),
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("{\"command\":\"echo hi\"}".into()),
            result_preview: result_preview.map(str::to_string),
        }
    }

    #[test]
    fn blocked_tool_records_fall_back_to_error_text_and_mark_rejected() {
        let rec = summary_tool_record(
            false,
            Some("blocked_tool: Explicit approval required: action scope is unbounded."),
            None,
        );
        assert_eq!(
            tool_record_result_text(&rec),
            "blocked_tool: Explicit approval required: action scope is unbounded."
        );
        assert!(tool_record_was_rejected(&rec));
    }

    #[test]
    fn executed_tool_records_prefer_result_preview() {
        let rec = summary_tool_record(false, Some("Error: command failed"), Some("stderr preview"));
        assert_eq!(tool_record_result_text(&rec), "stderr preview");
        assert!(!tool_record_was_rejected(&rec));
    }

    #[test]
    fn runtime_session_quality_assessment_uses_session_score_and_tools() {
        assert_eq!(
            build_runtime_session_quality_assessment("sess-9", 0.63, 7),
            SessionQualityAssessmentRequest {
                session_id: "sess-9".to_string(),
                score: 0.63,
                step_count: 7,
            }
        );
    }

    #[test]
    fn runtime_session_quality_assessment_saturates_large_tool_counts() {
        assert_eq!(
            build_runtime_session_quality_assessment("sess-9", 0.63, usize::MAX).step_count,
            i32::MAX
        );
    }

    fn tool_record(name: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 1,
            error: (!ok).then(|| "simulated error".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
        }
    }

    fn tool_result_row(name: &str, result: astra_tools::ToolResult) -> Value {
        let mut row = serde_json::Map::from_iter([
            (
                "tool_call_id".to_string(),
                Value::String(format!("{name}-call")),
            ),
            ("name".to_string(), Value::String(name.to_string())),
            ("result".to_string(), Value::String(result.output.clone())),
        ]);
        if let Some(fields) = result.metadata {
            row.extend(fields);
        }
        Value::Object(row)
    }

    #[test]
    fn refresh_runtime_promotion_signals_captures_live_query_failures_without_tools() {
        let existing = RuntimePromotionSignals {
            noise_filtered_quality: Some(ConfidenceInterval::exact(0.78)),
            ..RuntimePromotionSignals::default()
        };

        let updated = refresh_runtime_promotion_signals_from_turn(
            Some(&existing),
            "Check the latest git status",
            &["git_status".to_string()],
            2,
            &[],
            &[],
            &[],
            0.2,
        )
        .expect("recent turn signal should be captured");

        assert_eq!(
            updated.noise_filtered_quality,
            existing.noise_filtered_quality
        );
        let recent = updated.recent_turn.expect("recent turn");
        assert!(!recent.success);
        assert_eq!(recent.tool_call_count, 0);
        assert_eq!(recent.quality, ConfidenceInterval::exact(0.2));
    }

    #[test]
    fn refresh_runtime_promotion_signals_clears_stale_recent_turn_for_neutral_turns() {
        let existing = RuntimePromotionSignals::with_recent_turn_feedback(
            None,
            Some(RuntimeTurnEvaluationSignal::from_turn_evaluation(
                &crate::pipeline::evaluation::TurnEvaluation {
                    success: false,
                    quality: 0.22,
                    confidence: 0.72,
                    signals: Vec::new(),
                },
                2,
                1,
                true,
            )),
        )
        .expect("seed recent turn");

        let updated = refresh_runtime_promotion_signals_from_turn(
            Some(&existing),
            "Thanks!",
            &[],
            3,
            &[],
            &[],
            &[],
            0.0,
        );

        assert!(updated.is_none());
    }

    fn read_journal_events(session_id: &str) -> Vec<JournalEvent> {
        let writer = JournalWriter::new(session_id).unwrap();
        let content = std::fs::read_to_string(writer.path()).unwrap_or_default();
        content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn cleanup_journal(session_id: &str) {
        let writer = JournalWriter::new(session_id).unwrap();
        std::fs::remove_file(writer.path()).ok();
    }

    fn cleanup_session_artifacts(session_id: &str) {
        cleanup_journal(session_id);
        std::fs::remove_dir_all(
            astra_services::session_journal::local_sessions_dir().join(session_id),
        )
        .ok();
    }

    #[cfg(unix)]
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[cfg(unix)]
    fn set_env_var(key: &'static str, value: impl Into<std::ffi::OsString>) -> EnvVarGuard {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value.into());
        }
        EnvVarGuard { key, previous }
    }

    #[cfg(unix)]
    fn write_fake_mysql(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("mysql");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *"SELECT current_account_name() AS name"*)
    printf '+------+\n| name |\n+------+\n| sys  |\n+------+\n'
    ;;
  *"CREATE SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"RESTORE ACCOUNT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"DROP SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"UPDATE metrics SET value = 1"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"SELECT 1"*)
    printf '+---+\n| 1 |\n+---+\n| 1 |\n+---+\n'
    ;;
  *)
    printf 'Query OK, 1 row affected\n'
    ;;
esac
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    fn session_state_executor(
        session_id: &str,
        dir: &tempfile::TempDir,
        turn_index: u32,
    ) -> (
        crate::server::server_tool_executor::ServerToolExecutor,
        std::sync::Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>,
    ) {
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(session_id, "test-model");
        workspace.cwd = dir.path().display().to_string();
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let mut executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.to_string(),
            None,
            None,
        );
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple(session_id),
        ));
        session.write().unwrap().turn_number = turn_index;
        executor.set_observability_session(session.clone());
        executor.set_turn_index(turn_index);
        (executor, session)
    }

    #[tokio::test]
    async fn server_file_boundary_commits_successful_turn() {
        let session_id = format!("server-file-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(5);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            5,
            &[json!({"function": {"name": "write_file", "arguments": "{}"}})],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute("write_file", &json!({"path": "ok.txt", "content": "hello"}))
            .await;
        assert!(write_out.contains("Successfully wrote"));

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[tool_record("write_file", true)],
            &[],
        )
        .await;

        let events = read_journal_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );
        let detail = events[1].metadata.as_ref().unwrap()["execution_boundary"]["detail"].clone();
        assert_eq!(detail["executed_requests"].as_u64(), Some(1));
        assert_eq!(detail["file_entries_recorded"].as_u64(), Some(1));
        assert!(dir.path().join("ok.txt").exists());

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test]
    async fn server_file_boundary_aborts_and_rolls_back_failed_turn() {
        let session_id = format!("server-file-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(7);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            7,
            &[json!({"function": {"name": "write_file", "arguments": "{}"}})],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute(
                "write_file",
                &json!({"path": "turn.txt", "content": "hello"}),
            )
            .await;
        assert!(write_out.contains("Successfully wrote"));
        assert!(dir.path().join("turn.txt").exists());

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[tool_record("write_file", true), tool_record("grep", false)],
            &[],
        )
        .await;

        let events = read_journal_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );
        let boundary = &events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("grep"));
        assert_eq!(
            boundary["rollback"]["file_edits"]["reverted"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert!(!dir.path().join("turn.txt").exists());

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test]
    async fn server_git_boundary_aborts_and_reverts_failed_turn() {
        let session_id = format!("server-git-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "before").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "after").unwrap();

        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(8);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            8,
            &[json!({"function": {"name": "git_commit", "arguments": "{}"}})],
        )
        .expect("boundary should open for git_commit");

        let commit_result = executor
            .execute_with_metadata("git_commit", &json!({"message": "turn commit"}))
            .await;
        assert!(!commit_result.is_error, "got: {}", commit_result.output);

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[tool_record("git_commit", true), tool_record("grep", false)],
            &[tool_result_row("git_commit", commit_result)],
        )
        .await;

        let events = read_journal_events(&session_id);
        let boundary_events: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect();
        assert_eq!(boundary_events.len(), 2);
        let boundary = &boundary_events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("grep"));
        assert_eq!(
            boundary["rollback"]["git_mutations"]["reverted"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "before"
        );

        cleanup_session_artifacts(&session_id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_database_boundary_aborts_and_rolls_back_failed_turn() {
        let fake_bin = tempfile::TempDir::new().unwrap();
        write_fake_mysql(fake_bin.path());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(fake_bin.path().to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let _path_guard = set_env_var("PATH", joined);

        let session_id = format!("server-db-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(11);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            11,
            &[json!({"function": {"name": "mo_query", "arguments": "{}"}})],
        )
        .expect("boundary should open for mo_query");

        let query_out = executor
            .execute("mo_query", &json!({"sql": "UPDATE metrics SET value = 1"}))
            .await;
        assert!(query_out.contains("Query OK"), "got: {query_out}");

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[tool_record("mo_query", true), tool_record("grep", false)],
            &[],
        )
        .await;

        let events = read_journal_events(&session_id);
        let boundary_events: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect();
        assert_eq!(boundary_events.len(), 2);
        let boundary = &boundary_events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("grep"));
        assert_eq!(
            boundary["rollback"]["database_snapshots"]["restored"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test]
    async fn server_session_state_boundary_aborts_and_rolls_back_failed_turn() {
        let session_id = format!("server-session-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let (executor, session) = session_state_executor(&session_id, &dir, 9);
        let original_top_k = session.read().unwrap().config.memory.retrieval_top_k;
        let new_top_k = if original_top_k < 20 {
            original_top_k + 1
        } else {
            original_top_k.saturating_sub(1)
        };

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            9,
            &[json!({"function": {"name": "adjust_config", "arguments": "{}"}})],
        )
        .expect("boundary should open for adjust_config");

        let adjust_out = executor
            .execute(
                "adjust_config",
                &json!({"path": "memory.retrieval_top_k", "value": new_top_k}),
            )
            .await;
        let adjust_json: Value = serde_json::from_str(&adjust_out).unwrap();
        assert_eq!(adjust_json["status"].as_str(), Some("ok"));
        assert_eq!(
            session.read().unwrap().config.memory.retrieval_top_k,
            new_top_k
        );

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record("adjust_config", true),
                tool_record("grep", false),
            ],
            &[],
        )
        .await;

        let events = read_journal_events(&session_id);
        let boundary_events: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect();
        assert_eq!(boundary_events.len(), 2);
        let boundary = &boundary_events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("grep"));
        assert_eq!(
            boundary["rollback"]["session_state"]["restored"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            session.read().unwrap().config.memory.retrieval_top_k,
            original_top_k
        );

        cleanup_session_artifacts(&session_id);
    }
}
