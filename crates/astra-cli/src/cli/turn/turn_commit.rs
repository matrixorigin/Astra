//! Durable turn commit: journal, workspace, checkpoint, and sidecar persistence.

use std::time::Instant;

use super::turn_learning::TurnLearningSnapshot;
use crate::cli::session::session_lessons;
use crate::cli::session::session_side_effects::{
    build_bridge_pipeline_journal_events, enqueue_ingestion_events, enqueue_ingestion_pub,
};
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::{StreamResult, root_run_transcript_events};
use astra_services::session_journal;

fn cache_pending_context_assembly_trace(state: &mut SessionState, trace_json: &serde_json::Value) {
    match serde_json::from_value::<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>(
        trace_json.clone(),
    ) {
        Ok(trace) => state.latest_context_assembly_trace = Some(trace),
        Err(err) => {
            astra_core::agent_warn!("context_trace", "failed to cache context trace: {err}")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStallType<'a> {
    SignalStall,
    SkillLockout { skill: Option<&'a str> },
    Other,
}

impl<'a> TurnStallType<'a> {
    fn parse(raw: &'a str) -> Self {
        match raw {
            "sig_stall" => Self::SignalStall,
            "skill_lockout" => Self::SkillLockout { skill: None },
            value => value
                .strip_prefix("skill_lockout:")
                .map(|skill| Self::SkillLockout { skill: Some(skill) })
                .unwrap_or(Self::Other),
        }
    }

    fn confidence(self) -> f64 {
        match self {
            Self::SignalStall | Self::SkillLockout { .. } => 1.0,
            Self::Other => 0.0,
        }
    }
}

pub(crate) fn stall_type_confidence(stall_type: &str) -> f64 {
    TurnStallType::parse(stall_type).confidence()
}

#[derive(Default)]
struct TurnCommitIssues {
    messages: Vec<String>,
}

impl TurnCommitIssues {
    fn summary(&self) -> Option<String> {
        (!self.messages.is_empty()).then(|| self.messages.join("; "))
    }

    fn record_error(&mut self, action: &str, error: impl std::fmt::Display) {
        let message = format!("failed to {action}: {error}");
        astra_core::agent_warn!("turn_commit", "{message}");
        self.messages.push(message);
    }

    fn into_summary(self) -> Option<String> {
        self.summary()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnCommitOutcome {
    /// True when the primary turn event is durable (or no durable session exists).
    ///
    /// Only primary-journal failure makes this false. Deferred projection
    /// errors arrive through the managed post-commit completion channel.
    pub(crate) turn_persisted: bool,
    /// Summary of durable degradation encountered during commit.
    pub(crate) persistence_error: Option<String>,
}

#[derive(Debug)]
pub(crate) enum DeferredTurnSidecarError {
    Retryable(String),
    Permanent(String),
}

impl DeferredTurnSidecarError {
    fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable(message.into())
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self::Permanent(message.into())
    }

    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }
}

impl std::fmt::Display for DeferredTurnSidecarError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(message) | Self::Permanent(message) => formatter.write_str(message),
        }
    }
}

/// A sidecar projection is deliberately separate from the journal turn
/// boundary. It owns derived views plus immutable runtime evidence captured at
/// that boundary, so it can be serialized by the UI's post-commit queue
/// without borrowing mutable session state or delaying the next live turn.
pub(crate) struct DeferredTurnSidecarWork {
    projection_id: String,
    session_id: String,
    model: String,
    turn: u32,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    permission_mode: String,
    discovered_skills: Vec<String>,
    contract_state_json: Option<String>,
    sidecar_events: Vec<session_journal::JournalEvent>,
    turn_observability_events: Vec<session_journal::JournalEvent>,
    tools_used: Vec<String>,
    journal_dir_override: Option<std::path::PathBuf>,
}

const SIDECAR_PROJECTION_ID_KEY: &str = "sidecar_projection_id";
const SIDECAR_PROJECTION_INDEX_KEY: &str = "sidecar_projection_index";

fn tag_sidecar_projection_events(
    events: &mut [session_journal::JournalEvent],
    projection_id: &str,
) {
    fn insert_identity(
        metadata: &mut serde_json::Map<String, serde_json::Value>,
        projection_id: &str,
        index: usize,
    ) {
        metadata.insert(
            SIDECAR_PROJECTION_ID_KEY.into(),
            serde_json::Value::String(projection_id.to_string()),
        );
        metadata.insert(
            SIDECAR_PROJECTION_INDEX_KEY.into(),
            serde_json::json!(index),
        );
    }

    for (index, event) in events.iter_mut().enumerate() {
        let metadata = event
            .metadata
            .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        match metadata {
            serde_json::Value::Object(metadata) => {
                insert_identity(metadata, projection_id, index);
            }
            metadata => {
                let previous = std::mem::take(metadata);
                let mut replacement = serde_json::Map::new();
                replacement.insert("previous_metadata".into(), previous);
                insert_identity(&mut replacement, projection_id, index);
                *metadata = serde_json::Value::Object(replacement);
            }
        };
    }
}

fn completed_sidecar_projection_indices(
    session_id: &str,
    projection_id: &str,
) -> std::io::Result<std::collections::HashSet<usize>> {
    Ok(session_journal::read_journal(session_id)?
        .into_iter()
        .filter_map(|event| {
            let metadata = event.metadata?;
            let metadata = metadata.as_object()?;
            (metadata
                .get(SIDECAR_PROJECTION_ID_KEY)
                .and_then(serde_json::Value::as_str)
                == Some(projection_id))
            .then(|| {
                metadata
                    .get(SIDECAR_PROJECTION_INDEX_KEY)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
            })
            .flatten()
        })
        .collect())
}

impl DeferredTurnSidecarWork {
    fn from_live_turn(
        state: &SessionState,
        result: &StreamResult,
        line: &str,
        learning_snap: &TurnLearningSnapshot,
        turn_observability_events: Vec<session_journal::JournalEvent>,
    ) -> Option<Self> {
        let session_id = state.session_id.as_deref()?.to_string();
        // The root transcript is part of the primary journal commit, not a
        // derived projection. This worker owns only artifacts which may lag
        // without making a settled conversation disappear from Ctrl+O.
        let mut sidecar_events = Vec::new();
        if let Some((_internal_turn, trace_json)) = &result.pending_context_assembly_trace {
            sidecar_events.push(session_journal::JournalEvent::context_assembly_recorded(
                Some(&session_id),
                state.turn,
                trace_json.clone(),
            ));
        }
        extend_runtime_sidecar_events(&mut sidecar_events, state, line, result, learning_snap);

        Some(Self {
            projection_id: format!("turn-sidecar:{session_id}:{}", state.turn),
            session_id,
            model: astra_core::model_override::normalize_model_override(state.model.as_deref())
                .unwrap_or("unknown")
                .to_string(),
            turn: state.turn,
            total_prompt_tokens: state.total_prompt_tokens,
            total_completion_tokens: state.total_completion_tokens,
            total_cache_read_tokens: state.total_cache_read_tokens,
            total_cache_creation_tokens: state.total_cache_creation_tokens,
            permission_mode: state.perm_manager.mode().to_string(),
            discovered_skills: state.discovered_skills.iter().cloned().collect(),
            contract_state_json: state
                .durable_task_state
                .as_ref()
                .and_then(|durable| serde_json::to_string(&durable.contract).ok()),
            sidecar_events,
            turn_observability_events,
            tools_used: result.tools_used.clone(),
            journal_dir_override: session_journal::current_journal_dir_override(),
        })
    }

    /// Executes all derived local persistence after the primary journal event
    /// is durable. Callers must serialize work for a session to preserve the
    /// workspace/checkpoint order.
    #[tracing::instrument(
        target = "astra_cli::turn_commit",
        skip_all,
        fields(
            session_id = %self.session_id,
            turn = self.turn,
            projection_id = %self.projection_id,
            reconcile_existing
        )
    )]
    pub(crate) fn execute(&self, reconcile_existing: bool) -> Result<(), DeferredTurnSidecarError> {
        let _journal_dir_guard = self
            .journal_dir_override
            .as_deref()
            .map(session_journal::JournalDirGuard::new);
        let mut projection_errors = Vec::new();
        let mut sidecar_events = self.sidecar_events.clone();
        let recovered_workspace = match astra_services::session_workspace::read_workspace_optional(
            &self.session_id,
        ) {
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    %error,
                    "rebuilding deferred workspace projection after read failure"
                );
                astra_services::session_workspace::backup_invalid_workspace_file(
                        &self.session_id,
                    )
                    .map_err(|backup_error| {
                        DeferredTurnSidecarError::retryable(format!(
                            "failed to write workspace metadata: could not preserve invalid workspace before rebuilding: {backup_error}"
                        ))
                    })?;
                Some(astra_services::session_workspace::WorkspaceMetadata::new(
                    &self.session_id,
                    &self.model,
                ))
            }
        };
        let mut written_checkpoint = None;
        let workspace_write = astra_services::session_workspace::update_workspace(
            &self.session_id,
            || {
                recovered_workspace.unwrap_or_else(|| {
                    astra_services::session_workspace::WorkspaceMetadata::new(
                        &self.session_id,
                        &self.model,
                    )
                })
            },
            |workspace| {
                workspace.turn_count = workspace.turn_count.max(self.turn);
                workspace.total_tokens_in = workspace.total_tokens_in.max(self.total_prompt_tokens);
                workspace.total_tokens_out =
                    workspace.total_tokens_out.max(self.total_completion_tokens);
                workspace.total_cache_read_tokens = workspace
                    .total_cache_read_tokens
                    .max(self.total_cache_read_tokens);
                workspace.total_cache_creation_tokens = workspace
                    .total_cache_creation_tokens
                    .max(self.total_cache_creation_tokens);
                workspace.status = "active".to_string();
                workspace.updated_at = chrono::Utc::now().to_rfc3339();
                workspace.permission_mode = Some(self.permission_mode.clone());
                workspace.discovered_skills = self.discovered_skills.clone();
                workspace.contract_json = self.contract_state_json.clone();

                if astra_services::session_checkpoint::should_checkpoint(
                    self.turn,
                    astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
                ) {
                    let existing_checkpoint = workspace
                        .checkpoints
                        .iter()
                        .position(|turn| *turn == self.turn);
                    let checkpoint = astra_services::session_checkpoint::Checkpoint {
                        number: existing_checkpoint
                            .map(|index| index as u32 + 1)
                            .unwrap_or(workspace.checkpoints.len() as u32 + 1),
                        turn: self.turn,
                        title: format!("Turn {} checkpoint", self.turn),
                        summary: format!(
                            "Accumulated {} tokens ({} in, {} out). Tools: {}",
                            workspace.total_tokens_in + workspace.total_tokens_out,
                            workspace.total_tokens_in,
                            workspace.total_tokens_out,
                            self.tools_used.join(", "),
                        ),
                        tools_used: self.tools_used.clone(),
                        total_tokens: workspace.total_tokens_in + workspace.total_tokens_out,
                        had_stalls: false,
                        error_count: 0,
                        contract_state_json: workspace.contract_json.clone(),
                    };
                    if existing_checkpoint.is_some() {
                        sidecar_events.push(session_journal::JournalEvent::checkpoint(
                            Some(&self.session_id),
                            self.turn,
                            &checkpoint.summary,
                            checkpoint.total_tokens,
                            checkpoint.tools_used.len(),
                        ));
                    } else {
                        match astra_services::session_checkpoint::write_checkpoint(
                            &self.session_id,
                            &checkpoint,
                        ) {
                            Ok(_) => {
                                workspace.record_checkpoint();
                                sidecar_events.push(session_journal::JournalEvent::checkpoint(
                                    Some(&self.session_id),
                                    self.turn,
                                    &checkpoint.summary,
                                    checkpoint.total_tokens,
                                    checkpoint.tools_used.len(),
                                ));
                                written_checkpoint = Some(checkpoint);
                            }
                            Err(error) => projection_errors
                                .push(format!("failed to write session checkpoint: {error}")),
                        }
                    }
                }
                workspace.last_persistence_error =
                    (!projection_errors.is_empty()).then(|| projection_errors.join("; "));
            },
        );
        if let Err(error) = workspace_write {
            if let Some(checkpoint) = written_checkpoint.as_ref()
                && let Err(cleanup_error) = astra_services::session_checkpoint::remove_checkpoint(
                    &self.session_id,
                    checkpoint,
                )
            {
                tracing::warn!(
                    session_id = %self.session_id,
                    %cleanup_error,
                    "failed to remove checkpoint after workspace write failure"
                );
            }
            return Err(DeferredTurnSidecarError::retryable(format!(
                "failed to write workspace metadata: {error}"
            )));
        }

        let pipeline_events = match build_bridge_pipeline_journal_events(
            Some(&self.session_id),
            self.turn,
            &self.model,
            &self.turn_observability_events,
        ) {
            Ok(events) => events,
            Err(error) => {
                let error = format!("failed to build bridge pipeline journal events: {error}");
                projection_errors.push(error.clone());
                let detail = projection_errors.join("; ");
                if let Err(rewrite_error) =
                    astra_services::session_workspace::update_existing_workspace(
                        &self.session_id,
                        |workspace| {
                            workspace.last_persistence_error = Some(detail);
                            workspace.updated_at = chrono::Utc::now().to_rfc3339();
                        },
                    )
                {
                    tracing::warn!(
                        session_id = %self.session_id,
                        %rewrite_error,
                        "failed to persist deferred pipeline projection error in workspace"
                    );
                }
                return Err(DeferredTurnSidecarError::permanent(error));
            }
        };
        sidecar_events.extend(self.turn_observability_events.clone());
        sidecar_events.extend(pipeline_events);
        tag_sidecar_projection_events(&mut sidecar_events, &self.projection_id);
        if reconcile_existing {
            let completed =
                completed_sidecar_projection_indices(&self.session_id, &self.projection_id)
                    .map_err(|error| {
                        DeferredTurnSidecarError::retryable(format!(
                            "failed to reconcile turn sidecar projection: {error}"
                        ))
                    })?;
            sidecar_events.retain(|event| {
                event
                    .metadata
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                    .and_then(|metadata| metadata.get(SIDECAR_PROJECTION_INDEX_KEY))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .is_none_or(|index| !completed.contains(&index))
            });
        }

        let journal = session_journal::JournalWriter::new(&self.session_id).map_err(|error| {
            DeferredTurnSidecarError::retryable(format!("failed to open sidecar journal: {error}"))
        })?;
        if let Err(error) = journal.append_bulk(&sidecar_events) {
            let detail = format!("failed to append turn sidecar events: {error}");
            if let Err(rewrite_error) = astra_services::session_workspace::update_existing_workspace(
                &self.session_id,
                |workspace| {
                    workspace.last_persistence_error = Some(detail);
                    workspace.updated_at = chrono::Utc::now().to_rfc3339();
                },
            ) {
                tracing::warn!(
                    session_id = %self.session_id,
                    %rewrite_error,
                    "failed to persist deferred sidecar error in workspace"
                );
            }
            return Err(DeferredTurnSidecarError::retryable(format!(
                "failed to append turn sidecar events: {error}"
            )));
        }
        enqueue_ingestion_events(&sidecar_events);
        if projection_errors.is_empty() {
            Ok(())
        } else {
            Err(DeferredTurnSidecarError::retryable(
                projection_errors.join("; "),
            ))
        }
    }
}

fn extend_runtime_sidecar_events(
    sidecar_events: &mut Vec<session_journal::JournalEvent>,
    state: &SessionState,
    line: &str,
    result: &StreamResult,
    learning_snap: &TurnLearningSnapshot,
) {
    let latest_user_input = result.latest_user_input(line);
    for (stall_type, _) in &result.stall_events {
        let confidence = stall_type_confidence(stall_type);
        if confidence == 0.0 {
            continue;
        }
        let stall_event = session_journal::JournalEvent::stall_detected(
            state.session_id.as_deref(),
            state.turn,
            stall_type,
            0,
            confidence,
            &[],
        );
        sidecar_events.push(stall_event);
    }

    for verdict in &result.verdict_events {
        let verdict_event = session_journal::JournalEvent::turn_guard_verdict(
            state.session_id.as_deref(),
            state.turn,
            &verdict.severity,
            &verdict.injections,
            &verdict.avoid_tools,
            &verdict.health_avoidance_tools,
            verdict.advisory_threshold_reached,
            verdict.nudge_count,
            verdict.total_errors,
            verdict.total_timeouts,
            &verdict.timeout_dominant_tools,
            verdict.total_cache_hits,
            verdict.flaky_count,
        );
        sidecar_events.push(verdict_event);
    }

    let turn_eval_event = astra_runtime::pipeline::evaluation::build_turn_evaluation_journal_event(
        state.session_id.as_deref(),
        Some(state.turn),
        "cli_repl",
        &latest_user_input,
        &state.recent_tools,
        &result.tool_call_records,
        result.stall_events.len(),
        result.verdict_events.iter().any(|event| {
            event.severity.eq_ignore_ascii_case("warning")
                || event.severity.eq_ignore_ascii_case("critical")
        }),
        result.budget_pressure,
        &learning_snap.eval,
    );
    sidecar_events.push(turn_eval_event);
}

fn build_primary_turn_event(
    state: &SessionState,
    line: &str,
    result: &mut StreamResult,
    turn_start: Instant,
) -> (
    session_journal::JournalEvent,
    Vec<session_journal::JournalEvent>,
) {
    // Observability expansion can read a long journal and is therefore a
    // derived sidecar. Keep only the raw source events here; the serialized
    // post-commit worker enriches them after the primary turn is durable.
    let turn_observability_events = std::mem::take(&mut result.turn_observability_events);

    let effective_user_input = result.effective_user_input(line);
    let mut turn_event = session_journal::JournalEvent::turn(
        state.session_id.as_deref(),
        state.turn,
        astra_core::model_override::normalize_model_override(state.model.as_deref()),
        &effective_user_input,
        &result.full_text,
        result.tool_calls_count,
        result.prompt_tokens,
        result.completion_tokens,
        turn_start.elapsed().as_millis() as u64,
    )
    .with_tool_surface(
        std::mem::take(&mut result.visible_tools),
        std::mem::take(&mut result.selected_skills),
        result.tools_used.clone(),
        result.budget_used,
    )
    .with_run_id(result.run_id.as_deref())
    .with_tool_calls(result.tool_call_records.clone())
    .with_budget_pressure(result.budget_pressure)
    .with_plan_subtask(state.current_plan_subtask_id.as_deref())
    .with_ttft(result.ttft_ms)
    .with_context_time(result.context_ms)
    .with_routing_telemetry(
        result.routing_domain_hint.take(),
        result.entity_learn_skipped_no_domain,
    )
    .with_memoria_time(result.memoria_ms)
    .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);
    turn_event =
        turn_event.with_applied_user_intents(result.applied_user_intents.iter().map(|intent| {
            (
                intent.intent_id.as_str(),
                intent.delivery,
                intent.status,
                intent.event_index,
                intent.content.as_str(),
            )
        }));

    turn_event.llm_rounds = result.llm_rounds;
    let tool_ms: u64 = result
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed())
        .map(|record| record.ms)
        .sum();
    turn_event.total_tool_ms = Some(tool_ms);
    if let Some(duration) = turn_event.duration_ms {
        turn_event.total_llm_ms = Some(duration.saturating_sub(tool_ms));
    }
    if let Some(interruption) = result.interruption.as_ref() {
        turn_event.metadata = Some(merge_interruption_metadata(
            turn_event.metadata.take(),
            interruption,
        ));
    }

    (turn_event, turn_observability_events)
}

pub(crate) struct PrimaryTurnCommit {
    pub(crate) outcome: TurnCommitOutcome,
    pub(crate) deferred_sidecars: Option<DeferredTurnSidecarWork>,
}

pub(crate) fn commit_primary_turn(
    state: &mut SessionState,
    line: &str,
    result: &mut StreamResult,
    learning_snap: &TurnLearningSnapshot,
    turn_start: Instant,
) -> PrimaryTurnCommit {
    let has_stalls = !result.stall_events.is_empty();
    let mut issues = TurnCommitIssues::default();
    let mut turn_persisted = state.session_id.is_none();
    if let Some((_internal_turn, trace_json)) = &result.pending_context_assembly_trace {
        cache_pending_context_assembly_trace(state, trace_json);
    }

    if let Some(journal) = state.journal.as_ref() {
        // The root transcript is the same user-facing truth as the turn
        // boundary. Append both in one durable batch so a completed turn is
        // immediately visible to Ctrl+O even while derived projections are
        // queued behind slow workspace, CSL, or telemetry work.
        let (turn_event, turn_observability_events) =
            build_primary_turn_event(state, line, result, turn_start);
        let mut primary_events = vec![turn_event.clone()];
        primary_events.extend(root_run_transcript_events(
            state.session_id.as_deref(),
            result.run_id.as_deref(),
            &result.run_transcript_messages,
        ));

        turn_persisted = match journal.append_bulk(&primary_events) {
            Ok(()) => {
                state.last_turn_event = Some(turn_event.clone());
                for event in &primary_events {
                    enqueue_ingestion_pub(state, event);
                }
                true
            }
            Err(error) => {
                issues.record_error("append turn event", error);
                false
            }
        };

        let deferred_sidecars = turn_persisted
            .then(|| {
                DeferredTurnSidecarWork::from_live_turn(
                    state,
                    result,
                    line,
                    learning_snap,
                    turn_observability_events,
                )
            })
            .flatten();
        let persistence_error = issues.into_summary();
        state.session_persistence_error = persistence_error.clone();

        if has_stalls {
            session_lessons::checkpoint_lessons_from_runtime(state);
        }

        return PrimaryTurnCommit {
            outcome: TurnCommitOutcome {
                turn_persisted,
                persistence_error,
            },
            deferred_sidecars,
        };
    } else if state.session_id.is_some() {
        issues.record_error("append turn event", "session journal missing");
    }

    let persistence_error = issues.into_summary();
    state.session_persistence_error = persistence_error.clone();

    if has_stalls {
        session_lessons::checkpoint_lessons_from_runtime(state);
    }

    PrimaryTurnCommit {
        outcome: TurnCommitOutcome {
            turn_persisted,
            persistence_error,
        },
        deferred_sidecars: None,
    }
}

fn merge_interruption_metadata(
    existing: Option<serde_json::Value>,
    interruption: &serde_json::Value,
) -> serde_json::Value {
    let mut metadata = match existing {
        Some(serde_json::Value::Object(map)) => map,
        Some(value) => {
            let mut map = serde_json::Map::new();
            map.insert("previous_metadata".into(), value);
            map
        }
        None => serde_json::Map::new(),
    };
    metadata.insert("partial".into(), serde_json::json!(true));
    metadata.insert("interrupted".into(), serde_json::json!(true));
    if let Some(kind) = interruption.get("kind").and_then(|value| value.as_str()) {
        metadata.insert("interruption_kind".into(), serde_json::json!(kind));
    }
    metadata.insert("interruption".into(), interruption.clone());
    serde_json::Value::Object(metadata)
}
#[cfg(test)]
mod tests {
    use super::{commit_primary_turn, merge_interruption_metadata, stall_type_confidence};
    use crate::cli::session::session_state::SessionState;
    use crate::cli::turn::turn_learning::analyze_chat_turn_learning;
    use astra_services::session_journal;
    use serde_json::json;
    use std::time::Instant;

    fn commit_primary_and_project_sidecars(
        state: &mut SessionState,
        line: &str,
        result: &mut crate::cli::stream::streaming_types::StreamResult,
        learning: &crate::cli::turn::turn_learning::TurnLearningSnapshot,
        turn_start: Instant,
    ) -> super::TurnCommitOutcome {
        let primary = commit_primary_turn(state, line, result, learning, turn_start);
        let outcome = primary.outcome.clone();
        if let Some(sidecars) = primary.deferred_sidecars
            && let Err(error) = sidecars.execute(false)
        {
            state.session_persistence_error = Some(error.to_string());
        }
        outcome
    }

    #[test]
    fn primary_turn_commit_is_durable_before_derived_sidecars_run() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("primary-before-sidecars-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("durable answer");
        let learning = analyze_chat_turn_learning("inspect", state.turn, &[], &result);

        let primary = commit_primary_turn(
            &mut state,
            "inspect",
            &mut result,
            &learning,
            Instant::now(),
        );
        assert!(primary.outcome.turn_persisted);
        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::Turn)
        );
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::TurnEvaluation),
            "derived evaluation must not delay the primary journal boundary"
        );

        primary
            .deferred_sidecars
            .expect("durable turn should enqueue derived projections")
            .execute(false)
            .unwrap();
        let events = session_journal::read_journal(&sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::TurnEvaluation)
        );
    }

    #[test]
    fn deferred_sidecar_update_preserves_background_task_projection() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("sidecar-preserves-background-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("durable answer");
        let learning = analyze_chat_turn_learning("inspect", state.turn, &[], &result);
        let sidecars = commit_primary_turn(
            &mut state,
            "inspect",
            &mut result,
            &learning,
            Instant::now(),
        )
        .deferred_sidecars
        .expect("durable turn should enqueue derived projections");

        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.background_shell_tasks = vec![
            astra_services::session_workspace::BackgroundShellTaskProjection {
                id: "shell-1".into(),
                status: "running".into(),
                title: "make test-online".into(),
                started_at_ms: 1,
                ended_at_ms: None,
                stdout_path: "/tmp/shell-1.stdout".into(),
                stderr_path: "/tmp/shell-1.stderr".into(),
                exit_code: None,
                terminal_reason: None,
            },
        ];
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        sidecars.execute(false).unwrap();

        let persisted = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(persisted.background_shell_tasks.len(), 1);
        assert_eq!(persisted.turn_count, 1);
    }

    #[test]
    fn sidecar_reconciliation_is_idempotent_after_ambiguous_success() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("sidecar-reconcile-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("durable answer");
        let learning = analyze_chat_turn_learning("inspect", state.turn, &[], &result);
        let primary = commit_primary_turn(
            &mut state,
            "inspect",
            &mut result,
            &learning,
            Instant::now(),
        );
        let sidecars = primary.deferred_sidecars.expect("sidecar work");

        sidecars.execute(false).unwrap();
        sidecars.execute(true).unwrap();

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::TurnEvaluation
                })
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == session_journal::JournalEventType::Checkpoint)
                .count(),
            1
        );
        let workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(workspace.checkpoints, vec![state.turn]);
        assert_eq!(
            astra_services::session_checkpoint::read_checkpoint_index(&sid)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn commit_turn_persists_turn_evaluation_event() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-eval-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            recent_tools: vec!["git".into()],
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("Workspace is clean.");
        result.tools_used = vec!["git".into()];
        result.tool_calls_count = 1;
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git".into(),
            ok: true,
            ms: 12,
            error: None,
            input_bytes: Some(16),
            output_bytes: Some(240),
            args_preview: None,
            result_preview: Some("clean".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let learning =
            analyze_chat_turn_learning("git status", state.turn, &state.recent_tools, &result);
        commit_primary_and_project_sidecars(
            &mut state,
            "git status",
            &mut result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::TurnEvaluation)
            .expect("turn evaluation event");
        assert_eq!(event.turn, Some(1));
        let metadata = event.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "cli_repl");
        assert_eq!(metadata["live_query"], false);
        assert_eq!(metadata["success"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert_eq!(metadata["signal_count"], 2);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
        assert_eq!(metadata["signals"][1]["kind"], "all_tools_healthy");
    }

    #[test]
    fn primary_commit_persists_root_capture_before_deferred_sidecars() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-root-transcript-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("done");
        result.run_id = Some("run-root-turn-1".into());
        result.run_transcript_messages = vec![
            json!({"role": "user", "content": "inspect the scheduler"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call-1"}]}),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "scheduler is safe"}),
            json!({"role": "assistant", "content": "Done."}),
            json!({"role": "system", "content": "runtime context must not persist"}),
        ];
        let learning =
            analyze_chat_turn_learning("inspect the scheduler", state.turn, &[], &result);

        let primary = commit_primary_turn(
            &mut state,
            "inspect the scheduler",
            &mut result,
            &learning,
            Instant::now(),
        );
        assert!(primary.outcome.turn_persisted);

        let items = session_journal::read_journal(&sid)
            .expect("journal should be readable")
            .into_iter()
            .filter_map(|event| event.transcript_item)
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|item| item.run_id == "run-root-turn-1"));
        assert!(items.iter().all(|item| item.agent_id == "root"));
        assert_eq!(
            items.iter().map(|item| item.item_seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(items[0].message["content"], "inspect the scheduler");
        assert_eq!(items[1].message["tool_calls"][0]["id"], "call-1");
        assert_eq!(items[2].message["tool_call_id"], "call-1");
        assert_eq!(items[3].message["content"], "Done.");

        let append_order = session_journal::read_journal_append_order(&sid)
            .expect("append-ordered journal should be readable");
        let primary_turn = append_order
            .iter()
            .position(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("primary turn must be persisted");
        let first_transcript = append_order
            .iter()
            .position(|event| event.event_type == session_journal::JournalEventType::TranscriptItem)
            .expect("root transcript must be persisted");
        assert!(
            primary_turn < first_transcript,
            "root transcript must follow the primary turn in the same durable journal batch"
        );
        assert!(
            !append_order.iter().any(|event| {
                event.event_type == session_journal::JournalEventType::TurnEvaluation
            }),
            "derived evaluation must not be required before the transcript is readable"
        );
    }

    #[test]
    fn interrupted_success_turn_is_marked_partial_in_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-partial-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 7,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result(
            "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message.",
        );
        result.run_id = Some("run-budget-1".into());
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 3,
            "user_message": "[budget_exhausted] 3 tool call(s) completed. You can continue in the next message."
        }));
        result.tool_calls_count = 3;

        let learning = analyze_chat_turn_learning("continue", state.turn, &[], &result);
        commit_primary_and_project_sidecars(
            &mut state,
            "continue",
            &mut result,
            &learning,
            Instant::now(),
        );

        let event = state.last_turn_event.as_ref().expect("turn event");
        let metadata = event.metadata.as_ref().expect("partial metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interrupted"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);
        assert_eq!(metadata["run_id"], "run-budget-1");
    }

    #[test]
    fn interruption_metadata_preserves_non_object_previous_metadata() {
        let interruption = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
        });
        let metadata =
            merge_interruption_metadata(Some(serde_json::json!("legacy-metadata")), &interruption);
        assert_eq!(metadata["previous_metadata"], "legacy-metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        assert_eq!(metadata["interruption"]["resumable"], true);
    }

    #[test]
    fn interrupted_turn_replay_persists_observability_and_context_trace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-replay-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 4,
            ..Default::default()
        };
        let partial_text =
            "[budget_exhausted] 2 tool call(s) completed. You can continue in the next message.";
        let mut result = crate::tests::stub_stream_result(partial_text);
        result.prompt_tokens = 12_345;
        result.completion_tokens = 234;
        result.llm_rounds = Some(2);
        result.tool_calls_count = 2;
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "tool_calls_completed": 2,
            "user_message": partial_text,
        }));
        let mut llm_round = session_journal::JournalEvent::base_public(
            session_journal::JournalEventType::LlmRound,
            Some(&sid),
        );
        llm_round.turn = Some(4);
        llm_round.round = Some(1);
        llm_round.tokens_in = Some(12_345);
        llm_round.tokens_out = Some(234);
        llm_round.metadata = Some(serde_json::json!({
            "source": "agentic_loop",
            "finish_reason": "tool_calls",
        }));
        result.turn_observability_events = vec![llm_round];
        let mut trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
            turn_id: "turn-99".into(),
            session_id: sid.clone(),
            ..Default::default()
        };
        trace.tools.visible_tools = vec![
            astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "git".into(),
                tokens: 0,
            },
            astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "read_file".into(),
                tokens: 0,
            },
        ];
        trace.token_budget.total_used = 12_345;
        result.pending_context_assembly_trace = Some((99, trace.to_json_value()));

        let learning = analyze_chat_turn_learning("continue", state.turn, &[], &result);
        commit_primary_and_project_sidecars(
            &mut state,
            "continue",
            &mut result,
            &learning,
            Instant::now(),
        );

        let events = session_journal::read_journal(&sid).unwrap();
        let turn_event = events
            .iter()
            .find(|event| event.event_type == session_journal::JournalEventType::Turn)
            .expect("persisted turn event");
        let metadata = turn_event.metadata.as_ref().expect("turn metadata");
        assert_eq!(metadata["partial"], true);
        assert_eq!(metadata["interruption_kind"], "budget_exhausted");
        let cached_trace = state
            .latest_context_assembly_trace
            .as_ref()
            .expect("cached context trace");
        assert_eq!(cached_trace.turn_id, "turn-99");
        assert_eq!(cached_trace.token_budget.total_used, 12_345);
    }

    #[test]
    fn commit_turn_records_persistence_error_when_journal_append_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-journal-fail-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        std::fs::create_dir(writer.path()).unwrap();
        let mut state = SessionState {
            journal: Some(writer),
            session_id: Some(sid),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("journal append failure should degrade persistence state");
        assert!(error.contains("append turn event"), "{error}");
        assert!(
            astra_services::session_workspace::read_workspace_optional(
                state.session_id.as_deref().unwrap()
            )
            .expect("workspace lookup should not fail")
            .is_none(),
            "workspace metadata must not advance when the turn event was not journaled"
        );
    }

    #[test]
    fn commit_turn_does_not_record_missing_checkpoint_in_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-checkpoint-fail-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("checkpoints"), b"not-a-directory").unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("checkpoint write failure should degrade persistence state");
        assert!(error.contains("write session checkpoint"), "{error}");
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("workspace should still be written after checkpoint failure");
        assert!(
            persisted.checkpoints.is_empty(),
            "workspace must not reference a checkpoint file that was never written"
        );
        assert!(
            persisted
                .last_persistence_error
                .as_deref()
                .expect("checkpoint failure should be persisted in workspace")
                .contains("write session checkpoint")
        );
    }

    #[test]
    fn commit_turn_records_persistence_error_when_workspace_write_fails() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-workspace-fail-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(workspace_dir.parent().unwrap()).unwrap();
        std::fs::write(&workspace_dir, b"not-a-directory").unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("workspace write failure should degrade persistence state");
        assert!(error.contains("write workspace metadata"), "{error}");
    }

    #[test]
    fn commit_turn_records_bridge_pipeline_build_error_without_rolling_back_turn() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-bridge-fail-{}", uuid::Uuid::new_v4());
        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        std::fs::write(&journal_path, [0xff]).unwrap();
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        let outcome = commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        assert!(
            outcome.turn_persisted,
            "journal append should still succeed after bridge event construction failed"
        );
        let error = state
            .session_persistence_error
            .as_deref()
            .expect("bridge pipeline failure should degrade persistence state");
        assert!(
            error.contains("build bridge pipeline journal events"),
            "{error}"
        );
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("workspace should preserve bridge failure");
        assert!(
            persisted
                .last_persistence_error
                .as_deref()
                .unwrap_or_default()
                .contains("build bridge pipeline journal events")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_turn_removes_checkpoint_artifacts_when_workspace_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!(
            "test-turn-commit-checkpoint-rollback-{}",
            uuid::Uuid::new_v4()
        );
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        let checkpoint_dir = workspace_dir.join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let checkpoint_path = workspace_dir
            .join("checkpoints")
            .join("001-turn-5-checkpoint.md");
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("workspace write failure should degrade persistence state");
        assert!(error.contains("write workspace metadata"), "{error}");
        assert!(
            !checkpoint_path.exists(),
            "checkpoint file must be removed when workspace cannot reference it"
        );
        assert!(
            astra_services::session_checkpoint::read_checkpoint_index(&sid)
                .unwrap()
                .is_empty(),
            "checkpoint index must not reference rolled-back checkpoint"
        );
        let events = session_journal::read_journal(&sid).expect("journal should remain readable");
        assert!(
            !events.iter().any(|event| matches!(
                event.event_type,
                session_journal::JournalEventType::Checkpoint
            )),
            "rolled-back checkpoint must not be published as a journal sidecar"
        );
    }

    #[test]
    fn clean_commit_clears_stale_persistence_error() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-turn-commit-clear-{}", uuid::Uuid::new_v4());
        let mut state = SessionState {
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            session_id: Some(sid.clone()),
            model: Some("gpt-5".into()),
            turn: 1,
            session_persistence_error: Some("stale".into()),
            ..Default::default()
        };
        let mut result = crate::tests::stub_stream_result("hello");
        let learning = analyze_chat_turn_learning("hello", state.turn, &[], &result);

        commit_primary_and_project_sidecars(
            &mut state,
            "hello",
            &mut result,
            &learning,
            Instant::now(),
        );

        assert!(
            state.session_persistence_error.is_none(),
            "clean commit should clear stale persistence errors"
        );
        let persisted = astra_services::session_workspace::read_workspace(&sid)
            .expect("clean commit should refresh workspace metadata");
        assert!(
            persisted.last_persistence_error.is_none(),
            "successful commit should clear persisted degradation state"
        );
    }

    #[test]
    fn stall_type_confidence_maps_known_signals() {
        assert_eq!(stall_type_confidence("sig_stall"), 1.0);
        assert_eq!(stall_type_confidence("skill_lockout"), 1.0);
        assert_eq!(stall_type_confidence("skill_lockout:review-changes"), 1.0);
        assert_eq!(stall_type_confidence("repetition_stall"), 0.0);
        assert_eq!(stall_type_confidence("unknown_type"), 0.0);
    }
}
