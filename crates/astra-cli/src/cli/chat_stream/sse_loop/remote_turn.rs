//! One Server admission, with client-owned transport and durable projections.
//! No local model continuation, completion policy, or provider recovery loop.

use super::server_admission_host::CliServerAdmissionHost;
use astra_pipeline::step_protocol::{StepCheckpoint, WorkspaceObservationQuarantineV1};
use astra_pipeline::step_recorder::StepRecorder;
use astra_runtime::turn::agentic_loop::finalization::{
    TurnTraceFinalizationInput, UnattributedRecallRunBoundary,
    commit_session_continuity_checkpoint, finalize_session_turn_trace, journal_writer_for_owner,
    same_recovery_state,
};
use astra_runtime::turn::agentic_loop::host::{VolatileInjection, VolatileKind};
use astra_runtime::turn::run_control::RunStatusProvider;
use astra_services::session_journal::{JournalEvent, ToolCallRecord, TurnEventBuffer};
use astra_turn_core::agentic_turn_ingest::{
    AgenticTurnIngestMut, AgenticTurnIngestOutcome, ingest_remote_server_turn,
    terminal_assistant_message_to_append,
};
use astra_turn_core::chat_turn_sse_dispatch::{
    ServerLoopExecutionSummary, StreamAppliedUserIntent, TokenUsageCoverage,
};
use astra_turn_core::interruption::{
    InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
};
use astra_turn_core::turn_checkpoint::{TurnCheckpointInput, build_turn_continuity_checkpoint};
use astra_turn_core::turn_trace_collector::TurnTraceCollector;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

#[derive(Default)]
pub(super) struct RemoteTurnTelemetry {
    pub explain_analyze_events: Vec<astra_turn_types::ExplainAnalyzeEventV1>,
    pub explain_analyze_degraded: bool,
    pub first_ttft_ms: Option<u64>,
    pub context_compression_triggered: bool,
    pub all_tools_used: HashSet<String>,
    pub first_selection_report: Option<astra_turn_core::tool_registry_report::ToolSelectionReport>,
    pub first_budget_pressure: f64,
    pub first_context_assembly_ms: Option<u64>,
    pub first_memoria_ms: Option<u64>,
    pub all_selected_skills: Vec<String>,
    pub observability_session:
        Option<Arc<RwLock<astra_runtime::observability::ObservabilitySession>>>,
    pub observability_hub: Option<Arc<astra_runtime::observability::ObservabilityHub>>,
    pub turn_trace_collector: Option<TurnTraceCollector>,
    pub pending_context_assembly_trace: Option<(u32, Value)>,
}

#[derive(Default)]
pub(super) struct RemoteTurnAudit {
    pub server_terminal_unverified: bool,
    pub last_heavy_checkpoint: Option<StepCheckpoint>,
    pub compaction_state: Option<Value>,
    pub tool_call_records: Vec<ToolCallRecord>,
    pub workspace_observation_quarantine: Option<WorkspaceObservationQuarantineV1>,
}

pub(super) struct RemoteTurnSkills {
    pub resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    pub listing_message: Option<Value>,
    pub session_event_hooks: astra_skills::hooks::SessionEventHookRegistry,
}

/// A turn-scoped client projection. Execution truth is supplied by the Server.
/// It owns neither execution generation nor durable model continuation.
pub(super) struct RemoteTurnState {
    pub messages: Vec<Value>,
    pub run_transcript_capture: Vec<Value>,
    pub current_session_id: Option<String>,
    pub current_run_id: Option<String>,
    pub context_manifest_user_id: Option<String>,
    pub context_manifest_model_name: Option<String>,
    pub final_text: String,
    pub final_text_model_item_id: Option<String>,
    pub total_prompt: u64,
    pub total_completion: u64,
    pub total_cache_read: u64,
    pub total_cache_creation: u64,
    pub total_tool_calls: u32,
    pub total_observation_tool_calls: u32,
    pub has_any_usage: bool,
    pub qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
    pub last_finish_reason: Option<String>,
    pub llm_rounds_completed: u32,
    pub tool_health: astra_turn_core::tool_health::ToolHealthTracker,
    pub restricted_tools: HashSet<String>,
    pub step_recorder: StepRecorder,
    pub stall: RemoteTurnAudit,
    pub telemetry: RemoteTurnTelemetry,
    pub skills: RemoteTurnSkills,
    pub applied_user_intents: Vec<StreamAppliedUserIntent>,
    pub volatile_pending: Vec<VolatileInjection>,
    pub run_control: Option<Arc<crate::cli::turn::local_run_control::LocalRunControl>>,
    pub cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    pub pipeline_session: Option<astra_turn_core::pipeline_session::PipelineSession>,
    pub recent_tools: Vec<String>,
    pub deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
    pub last_measured_prompt_tokens: Option<u64>,
    pub consecutive_context_window_errors: u32,
    pub max_turn_input_tokens: u64,
    pub permission_context: Option<super::RootPermissionContextHandle>,
    pub interruption: Option<InterruptionRecord>,
    pub approval_overrides: Option<astra_turn_core::approval_fingerprint::FingerprintedOverrides>,
    pub session_turn: u32,
    pub canonical_turn_chain_id: Option<String>,
    pub root_user_query_event_id: Option<String>,
    pub turn_event_buffer: Option<TurnEventBuffer>,
    pub remote_summary: Option<ServerLoopExecutionSummary>,
    pub server_terminal_authoritative: bool,
    pub local_input_run_id: String,
    pub stop_hook_prompt: Option<Value>,
    pub harness: astra_runtime::turn::harness_adapter::HarnessSlot,
}

impl RemoteTurnState {
    pub fn current_model_identity(&self) -> Option<&str> {
        self.context_manifest_model_name.as_deref()
    }

    pub fn begin_run_transcript_capture(&mut self, items: impl IntoIterator<Item = Value>) {
        self.run_transcript_capture.extend(items);
    }

    pub fn record_prompt_history_messages(&mut self, items: impl IntoIterator<Item = Value>) {
        self.run_transcript_capture.extend(items);
    }

    pub fn take_run_transcript_capture(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.run_transcript_capture)
    }

    pub fn push_volatile(&mut self, kind: VolatileKind, text: impl Into<String>) {
        self.push_volatile_payload(kind, Value::String(text.into()));
    }

    pub fn push_volatile_payload(&mut self, kind: VolatileKind, mut payload: Value) {
        if let Value::String(text) = &mut payload {
            *text = text.trim().to_string();
            if text.is_empty() {
                return;
            }
        }
        if kind.is_singleton() {
            self.volatile_pending.retain(|item| item.kind != kind);
        }
        self.volatile_pending.push(VolatileInjection {
            kind,
            payload,
            round_index: 0,
            attempt_leased: false,
        });
    }

    pub fn token_usage_coverage(&self) -> TokenUsageCoverage {
        self.remote_summary
            .as_ref()
            .map(|summary| {
                summary.token_usage_coverage.unwrap_or(TokenUsageCoverage {
                    attempts: summary.llm_rounds,
                    provider_reported: 0,
                    unavailable: summary.llm_rounds,
                })
            })
            .unwrap_or_default()
    }

    fn capture_appended_messages(&mut self, start: usize) {
        for message in &mut self.messages[start..] {
            if let Some(chain) = self.canonical_turn_chain_id.as_deref() {
                astra_turn_types::mark_turn_message(message, chain);
            }
        }
        self.run_transcript_capture
            .extend(self.messages[start..].iter().cloned());
    }

    fn interruption_summary(&self, detail: String) -> InterruptionStateSummary {
        InterruptionStateSummary {
            has_checkpoint: self.stall.last_heavy_checkpoint.is_some(),
            tool_calls_completed: self.total_tool_calls,
            turns_completed: self.llm_rounds_completed,
            remaining_turns: 0,
            error_detail: Some(detail),
            stall_signal: None,
            resume_restricted_tools: self.restricted_tools.iter().cloned().collect(),
        }
    }

    fn checkpoint(&mut self) {
        let Some(session_id) = self
            .current_session_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let Some(owner) = self
            .context_manifest_user_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let Some(mut heavy) = build_turn_continuity_checkpoint(TurnCheckpointInput {
            step_recorder: &self.step_recorder,
            messages: &self.messages,
            max_turn_input_tokens: self.max_turn_input_tokens,
            last_measured_prompt_tokens: self.last_measured_prompt_tokens,
            remaining_turns: 0,
            restricted_tools: &self.restricted_tools,
            recent_tools: &self.recent_tools,
            interruption: self.interruption.as_ref(),
            approval_overrides: self.approval_overrides.as_ref(),
            consecutive_context_window_errors: self.consecutive_context_window_errors,
            deferred_tool_activations: &mut self.deferred_tool_activations,
            pipeline_session: self.pipeline_session.as_ref(),
            workspace_observation_quarantine: self.stall.workspace_observation_quarantine.as_ref(),
        }) else {
            return;
        };
        heavy.compaction_state = self.stall.compaction_state.clone();
        let checkpoint = StepCheckpoint::Heavy(Box::new(heavy));
        if self
            .stall
            .last_heavy_checkpoint
            .as_ref()
            .is_some_and(|old| same_recovery_state(old, &checkpoint))
        {
            return;
        }
        // Only the root CLI entrance with caller-provided durable journals
        // installs context_manifest_user_id. Utility/subrun calls do not publish.
        if let Some((checkpoint, _snapshot)) = commit_session_continuity_checkpoint(
            owner,
            session_id,
            self.session_turn.max(1),
            checkpoint,
            None,
        ) {
            self.stall.last_heavy_checkpoint = Some(checkpoint);
        }
    }

    #[cfg(feature = "harness")]
    fn harness_at(&self, point: astra_harness::HookPoint) -> astra_harness::HookVerdict {
        self.harness.fire(
            point,
            astra_runtime::turn::harness_adapter::HarnessSnapshotInput {
                session_id: self.current_session_id.as_deref(),
                round_index: 0,
                turns_used: self.llm_rounds_completed,
                turns_limit: None,
                settlement_rounds_reserved: Some(0),
                session_turn: self.session_turn,
                prompt_tokens: self.total_prompt,
                completion_tokens: self.total_completion,
                cache_read_tokens: self.total_cache_read,
                cache_creation_tokens: self.total_cache_creation,
                input_budget_tokens: self.max_turn_input_tokens,
                measured_prompt_tokens: self.last_measured_prompt_tokens,
                message_count: self.messages.len(),
                tool_calls: self.total_tool_calls,
                tools_used: &self.telemetry.all_tools_used,
                tool_signatures: &[],
                final_text: &self.final_text,
                interruption: self.interruption.as_ref(),
                tool_records: &self.stall.tool_call_records,
                read_only_round_streak: 0,
                delegations: 0,
                recursion_depth: 0,
                consecutive_errors: 0,
                causal_chain_id: self.canonical_turn_chain_id.as_deref(),
            },
        )
    }
}

/// The executor is shared across turns; clear request-only channels even when
/// this future is dropped during a tool callback.
struct InteractionChannels(Arc<crate::edge_tools::ToolExecutor>);
impl Drop for InteractionChannels {
    fn drop(&mut self) {
        self.0.set_ask_user_request_tx(None);
        self.0.set_plan_review_request_tx(None);
    }
}

async fn prepare_remote_request(
    host: &mut CliServerAdmissionHost<'_>,
    state: &mut RemoteTurnState,
) -> Result<(), astra_core::ClassifiedError> {
    let read_only = if let Some(context) = state.permission_context.as_ref() {
        context.read().await.inherited.read_only_execution
    } else {
        false
    };
    if read_only {
        state.skills.session_event_hooks.disable_execution();
    }
    if state
        .skills
        .session_event_hooks
        .has_event(astra_skills::hooks::SessionEvent::SessionStart)
    {
        let output = astra_skills::hooks::evaluate_session_hooks(
            &state.skills.session_event_hooks,
            astra_skills::hooks::SessionEvent::SessionStart,
            state.current_session_id.as_deref().unwrap_or(""),
            Some(host.message),
        )
        .await;
        if let Some(context) = output.context {
            state.push_volatile_payload(
                VolatileKind::SessionHookContext,
                json!({"event":"session_start","context":context}),
            );
        }
        for (key, value) in output.env_vars {
            astra_core::session_env_overlay::set(&key, &value);
            state.skills.session_event_hooks.note_environment_applied();
        }
    }
    if (state.telemetry.observability_session.is_some() || state.skills.resolver.is_some())
        && std::env::var("ASTRA_CAPTURE_TRACES")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
    {
        state.telemetry.turn_trace_collector = Some(TurnTraceCollector::new(
            format!("turn-{}", state.session_turn),
            state.current_session_id.clone().unwrap_or_default(),
        ));
    }
    if let Some(resolver) = state.skills.resolver.as_ref() {
        let skills = resolver.available_skills();
        if !skills.is_empty() {
            host.inject_tool_schema(astra_runtime::turn::skill_tool::skill_tool_schema_v2());
            let edge_skills = skills.iter().map(|skill| json!({"name":skill.name,"version":null,"description":skill.description,"when_to_use":skill.when_to_use,"aliases":skill.aliases})).collect::<Vec<_>>();
            state.skills.listing_message = astra_runtime::prompts::build_skill_listing_section_with_context_window_and_caps(&skills,
                (state.max_turn_input_tokens > 0).then_some(state.max_turn_input_tokens.saturating_mul(10).div_ceil(8).min(u64::from(u32::MAX)) as u32),
                host.capabilities.has(astra_turn_core::capability::Capability::AgentSpawner))
                .map(|section| json!({"role":"system","content":section.text,"edge_skills":edge_skills}));
        }
    }
    if let Some(content) = state.stop_hook_prompt.take().and_then(|prompt| {
        prompt
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }) {
        state.push_volatile(VolatileKind::StopHookEvidence, content);
    }
    state
        .step_recorder
        .begin_turn_with_context(state.session_turn.max(1), 0);
    state.turn_event_buffer = Some(TurnEventBuffer::begin_turn(
        state.current_session_id.as_deref(),
        state.session_turn.max(1),
    ));
    if let (Some(hub), Some(session)) = (
        &state.telemetry.observability_hub,
        &state.telemetry.observability_session,
    ) {
        let owner = astra_core::sync_poison::recover_rwlock_read(session)
            .user_id
            .clone();
        astra_runtime::observability::on_turn_start(
            hub,
            state.current_session_id.as_deref().unwrap_or(""),
            &owner,
            host.message,
        );
    }
    Ok(())
}

/// Drive exactly one Server-owned execution stream. Outer turn owners retain
/// input restoration, durable cancellation, UI handoff and final journal commit.
pub(super) async fn consume_remote_turn(
    host: &mut CliServerAdmissionHost<'_>,
    state: &mut RemoteTurnState,
) -> Result<(), astra_core::ClassifiedError> {
    let _recalls = UnattributedRecallRunBoundary::new(host.executor.memory_recall_scope());
    let _channels = InteractionChannels(Arc::clone(&host.executor));
    let result = consume_remote_turn_inner(host, state).await;
    let rejected = super::is_pre_admission_rejection(
        host.last_error_code.as_deref(),
        host.last_error_metadata.as_ref(),
        host.last_physical_run_id.as_deref(),
    );
    if rejected {
        state.turn_event_buffer = None;
        state.step_recorder.discard_uncommitted();
        state.interruption = None;
    } else {
        if let Err(error) = &result {
            state.step_recorder.end_turn(false);
            if state.interruption.is_none()
                && error.kind != astra_core::ErrorKind::Cancelled
                && let Some((kind, action)) =
                    astra_turn_core::interruption::interruption_from_error_kind(error.kind)
            {
                state.interruption = Some(InterruptionRecord::new(
                    kind,
                    action,
                    state.interruption_summary(error.message.clone()),
                ));
            }
        }
        let cancelled = result
            .as_ref()
            .err()
            .is_some_and(|error| error.kind == astra_core::ErrorKind::Cancelled);
        if cancelled {
            use astra_turn_core::orchestration_types::CancellationOrigin;
            let owner = crate::cli::cli_config::cli_utils::cli_user_id();
            let origin = if let Some(control) = state.run_control.as_ref() {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    control.cancellation_origin(&owner, &state.local_input_run_id),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(CancellationOrigin::Unverified)
            } else {
                CancellationOrigin::Runtime
            };
            let reason = match origin {
                CancellationOrigin::User => "parent turn cancelled by user",
                CancellationOrigin::Runtime => "parent execution cancelled by runtime",
                CancellationOrigin::Unverified => {
                    astra_runtime::orchestration::CANCELLATION_ORIGIN_UNVERIFIED
                }
            };
            host.cancel_child_agents(&[], reason, origin).await;
            if origin == CancellationOrigin::User && state.interruption.is_none() {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::UserCancelled,
                    ResumeAction::ContinueImmediately,
                    state.interruption_summary(reason.into()),
                ));
            }
        }
        if result.is_err() {
            finalize_remote_trace(state).await;
            state.checkpoint();
        }
        if (result.is_err() || state.interruption.is_some())
            && let Some(session) = state.current_session_id.as_deref()
        {
            if let Some(buffer) = state.turn_event_buffer.as_mut()
                && !buffer.is_empty()
                && let Err(error) =
                    buffer.flush_for_owner(state.context_manifest_user_id.as_deref(), session, true)
            {
                tracing::warn!(session_id=session,%error,"failed to flush interrupted remote turn journal");
            }
            if let Some(interruption) = state.interruption.as_ref() {
                let event = JournalEvent::interruption_recorded(
                    Some(session),
                    state.session_turn.max(1),
                    interruption.to_json(),
                )
                .with_agentic_step(Some(1));
                if let Ok(writer) =
                    journal_writer_for_owner(state.context_manifest_user_id.as_deref(), session)
                    && let Err(error) = writer.append(&event)
                {
                    tracing::warn!(session_id=session,%error,"failed to append remote turn interruption");
                }
            }
        }
    }
    #[cfg(feature = "harness")]
    state.harness_at(astra_harness::HookPoint::SessionEnd);
    host.on_turn_completed(state);
    result
}

async fn consume_remote_turn_inner(
    host: &mut CliServerAdmissionHost<'_>,
    state: &mut RemoteTurnState,
) -> Result<(), astra_core::ClassifiedError> {
    prepare_remote_request(host, state).await?;
    #[cfg(feature = "harness")]
    for point in [
        astra_harness::HookPoint::SessionStart,
        astra_harness::HookPoint::PreLlmRequest,
    ] {
        let interrupted = match state.harness_at(point) {
            astra_harness::HookVerdict::Block { reason } => {
                Some((InterruptionKind::HarnessBlocked, reason))
            }
            astra_harness::HookVerdict::Pause { reason, .. } => {
                Some((InterruptionKind::HarnessPaused, reason))
            }
            astra_harness::HookVerdict::Continue => None,
        };
        if let Some((kind, reason)) = interrupted {
            state.interruption = Some(InterruptionRecord::new(
                kind,
                if kind.is_resumable() {
                    ResumeAction::ContinueImmediately
                } else {
                    ResumeAction::StartNewSession
                },
                state.interruption_summary(reason),
            ));
            state.final_text = state
                .interruption
                .as_ref()
                .expect("installed interruption")
                .user_message
                .clone();
            host.render_final_text(&state.final_text, None);
            host.on_final_output_ready().await;
            state.step_recorder.end_turn(false);
            settle_remote_projection(state).await;
            return Ok(());
        }
    }
    if state
        .cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "turn cancelled before Server admission",
        ));
    }
    // The local notification owner retains these facts across auth/session
    // retries. Durable guidance and its acknowledgements stay on the Server.
    if let Some(control) = state.run_control.as_ref() {
        let content = control.runtime_notifications_for_request().join("\n\n");
        state.push_volatile(VolatileKind::RuntimeInputBoundary, content);
    }
    let mut turn = host.fetch_remote_turn(state).await?;
    // Observe exact callback receipts even if the stream ends in failure.
    let mut seen = HashSet::new();
    for callback in &turn.edge_tool_round {
        if seen.insert(callback.request_id.clone()) {
            state.stall.tool_call_records.push(
                astra_turn_core::headless_tool_journal::journal_record_edge_tool_result(callback),
            );
        }
    }
    if state.stall.workspace_observation_quarantine.is_none() {
        state.stall.workspace_observation_quarantine =
            astra_runtime::turn::agentic_loop::workspace_observation_quarantine_from_records(
                &state.stall.tool_call_records,
            );
        if state.stall.workspace_observation_quarantine.is_some() {
            state.checkpoint();
        }
    }
    for observation in &turn.core.context_compactions {
        if let Some(event) = observation.record_projection(
            state.max_turn_input_tokens,
            &mut state.step_recorder,
            state.pipeline_session.as_mut(),
        ) {
            state.telemetry.context_compression_triggered = true;
            host.on_compaction(event);
        }
    }
    let start = state.messages.len();
    let outcome = ingest_remote_server_turn(
        &turn.core,
        turn.ttft_ms,
        turn.core.error_kind,
        host.message,
        AgenticTurnIngestMut {
            model_item_id: turn.core.model_item_id.as_deref(),
            final_text_model_item_id: &mut state.final_text_model_item_id,
            first_ttft_ms: &mut state.telemetry.first_ttft_ms,
            current_session_id: &mut state.current_session_id,
            current_run_id: &mut state.current_run_id,
            final_text: &mut state.final_text,
            last_finish_reason: &mut state.last_finish_reason,
            total_prompt: &mut state.total_prompt,
            total_completion: &mut state.total_completion,
            total_cache_read: &mut state.total_cache_read,
            total_cache_creation: &mut state.total_cache_creation,
            total_tool_calls: &mut state.total_tool_calls,
            total_observation_tool_calls: &mut state.total_observation_tool_calls,
            step_recorder: &mut state.step_recorder,
            all_tools_used: &mut state.telemetry.all_tools_used,
            has_any_usage: &mut state.has_any_usage,
            messages: &mut state.messages,
            last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
            consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
        },
    );
    state.capture_appended_messages(start);
    state.qualified_usage = turn.core.qualified_usage;
    state.llm_rounds_completed = turn
        .core
        .server_execution_summary
        .as_ref()
        .map_or(0, |summary| summary.llm_rounds);
    state.remote_summary = turn.core.server_execution_summary.take();
    if let Some(session) = state.current_session_id.as_deref() {
        host.on_session_bound(session);
        if let Some(buffer) = state.turn_event_buffer.as_mut()
            && let Err(error) = buffer.bind_session_id(session)
        {
            tracing::warn!(%error,"failed to bind remote turn events to session");
        }
    }
    if let Some(collector) = state.telemetry.turn_trace_collector.as_ref() {
        if let Some(identity) = turn
            .core
            .context_manifest_trace
            .as_ref()
            .and_then(|trace| trace.get("request_identity"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
        {
            collector.record_request_identity(identity);
        }
        if let Some(tokens) = turn.core.system_prompt_tokens {
            collector.set_system_prompt_tokens(tokens);
        }
        if let Some(breakdown) = turn
            .core
            .system_prompt_breakdown
            .as_ref()
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        {
            collector.record_system_prompt(breakdown);
        }
    }
    match outcome {
        AgenticTurnIngestOutcome::Fatal(error) => return Err(error),
        AgenticTurnIngestOutcome::Break => {}
        _ => unreachable!("remote ingest never transfers continuation authority"),
    }
    if let Some(summary) = state.remote_summary.as_ref()
        && let Some(frame) = summary.runtime_feedback_for(
            state.current_session_id.as_deref(),
            state.current_run_id.as_deref(),
            state.current_model_identity(),
            state.session_turn.max(1),
        )
    {
        if let Some(pipeline) = state.pipeline_session.as_mut()
            && pipeline.accept_authoritative_runtime_feedback(frame)
        {
            if let Some(buffer) = state.turn_event_buffer.as_mut() {
                let feedback =
                    astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(frame);
                if let Ok(payload) = serde_json::to_value(feedback) {
                    buffer.record(
                        JournalEvent::pipeline_feedback(
                            state.current_session_id.as_deref(),
                            state.session_turn.max(1),
                            payload,
                        )
                        .with_producer_scope(state.current_run_id.as_deref()),
                    );
                }
                for audit in pipeline.drain_pending_audits() {
                    if let Ok(payload) = serde_json::to_value(audit) {
                        buffer.record(
                            JournalEvent::pipeline_compaction_audit(
                                state.current_session_id.as_deref(),
                                state.session_turn.max(1),
                                payload,
                            )
                            .with_producer_scope(state.current_run_id.as_deref()),
                        );
                    }
                }
            }
        }
    }
    // Append only missing canonical terminal content, preserving model identity.
    // The render policy owns whether terminal text is already visible.
    if let Some(message) = terminal_assistant_message_to_append(
        &state.messages,
        &state.final_text,
        state.final_text_model_item_id.as_deref(),
    ) {
        let start = state.messages.len();
        state.messages.push(message);
        state.capture_appended_messages(start);
    }
    if !state.final_text.is_empty() {
        host.render_final_text(&state.final_text, state.final_text_model_item_id.as_deref());
        host.on_final_output_ready().await;
    }
    state.step_recorder.end_turn(state.interruption.is_none());
    settle_remote_projection(state).await;
    Ok(())
}

async fn finalize_remote_trace(state: &mut RemoteTurnState) {
    if let Some(collector) = state.telemetry.turn_trace_collector.take() {
        finalize_session_turn_trace(TurnTraceFinalizationInput {
            collector,
            session_id: state.current_session_id.as_deref(),
            session_turn: state.session_turn.max(1),
            max_turn_input_tokens: state.max_turn_input_tokens,
            last_measured_prompt_tokens: state.last_measured_prompt_tokens,
            context_compression_triggered: state.telemetry.context_compression_triggered,
            first_budget_pressure: &mut state.telemetry.first_budget_pressure,
            pending_context_assembly_trace: &mut state.telemetry.pending_context_assembly_trace,
            observability_session: state.telemetry.observability_session.clone(),
            persistence: None,
        })
        .await;
    }
}

async fn settle_remote_projection(state: &mut RemoteTurnState) {
    finalize_remote_trace(state).await;
    if let Some(pipeline) = state.pipeline_session.as_mut() {
        pipeline
            .working_memory_mut()
            .apply_turn_settlement(state.interruption.as_ref());
    }
    #[cfg(feature = "harness")]
    state.harness_at(astra_harness::HookPoint::SessionEnd);
    state.checkpoint();
}
