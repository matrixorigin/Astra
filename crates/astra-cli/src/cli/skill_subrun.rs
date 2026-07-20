//! Fork skill sub-run executor for the CLI.
//!
//! When a skill has `execution_context: Fork`, the agentic loop delegates
//! execution to an [`IsolatedSkillExecutor`] which wraps a [`SkillSubRunExecutor`].
//! This module provides the CLI implementation that runs a separate agentic loop
//! using the same API and tool infrastructure as the parent conversation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::agentic::headless_round::HeadlessStderrStyle,
    turn::agentic_loop::finalization::run_agentic_loop_with_host,
    turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CancellationState, HostTurnResult,
        SkillState, StopHookState, TurnInteractionMode, TurnInteractionPolicy,
        interaction_scoped_tool_restrictions, project_skill_subrun_outcome,
        runtime_manifest_for_model,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_edge_profile_extensions,
        set_payload_tool_results_if_non_empty,
    },
    turn::tool_schema_prune::inject_required_tool_names,
    turn::turn_guard::TurnGuard,
};
use astra_skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use astra_turn_core::tool::schema::tool_names_from_schemas;
use serde_json::{Value, json};

use super::effects::ChatTurnPrepLineGuard;
use super::permission_manager::PermissionManager;
use crate::cli::chat_stream::turn_policy_from_payload_edge_tools;
use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::cli::stream::stream_render::{EdgeSseContext, RenderPolicy, consume_turn_sse};
use crate::edge_tools;

const SUBRUN_MAX_TURNS: usize = 25;

/// Cumulative token budget for skill subruns.
/// Caps total (prompt + completion) across all rounds to prevent runaway cost.
const SUBRUN_MAX_CUMULATIVE_TOKENS: u64 = 120_000;

pub(crate) async fn resolve_subrun_model_selection(
    api: &astra_thin_client::ThinClient,
    token: &str,
    model: Option<&str>,
) -> Result<crate::cli::session::session_runtime::ServerModelSelection, String> {
    if let Some(model) = model {
        return crate::cli::session::session_runtime::resolve_server_model_selection(
            api, token, model,
        )
        .await;
    }
    match crate::cli::session::session_runtime::resolve_server_default_model(api, token).await {
        crate::cli::session::session_runtime::ServerDefaultModel::Selected(selection) => {
            Ok(selection)
        }
        crate::cli::session::session_runtime::ServerDefaultModel::NoModels => {
            Err("no active model Offering is available for the sub-run".to_string())
        }
        crate::cli::session::session_runtime::ServerDefaultModel::Unavailable => {
            Err("the Server model registry is unavailable for the sub-run".to_string())
        }
    }
}

// ─── SubRunHost ──────────────────────────────────────────────────────────────

/// Minimal agentic loop host for fork sub-runs.
///
/// Owns all resources so it doesn't borrow from a parent scope. Runs in quiet
/// mode with all terminal rendering suppressed.
///
/// Shared between skill sub-runs and delegate sub-runs.
pub(crate) struct SubRunHost {
    pub(crate) api: astra_thin_client::ThinClient,
    pub(crate) token: String,
    pub(crate) model: Option<String>,
    pub(crate) offering_id: String,
    pub(crate) project_root: PathBuf,
    pub(crate) executor: std::sync::Arc<edge_tools::ToolExecutor>,
    pub(crate) all_schemas: Vec<Value>,
    pub(crate) valid_tool_names: HashSet<String>,
    pub(crate) perm_manager: PermissionManager,
    /// Shared journal writer from the parent session. When present,
    /// child LLM rounds are written to the parent's journal with an
    /// `agent_id` tag so the unified timeline can interleave them.
    pub(crate) journal: Option<std::sync::Arc<astra_services::session_journal::JournalWriter>>,
    /// Stable parent-owned identity for local child transcript persistence.
    /// Temporary server chat identities must never be used for this journal.
    pub(crate) journal_identity: Option<SubRunJournalIdentity>,
    /// Per-response completion token limit from the skill manifest.
    pub(crate) max_completion_tokens: Option<u32>,
    /// Effort level from the skill manifest.
    pub(crate) effort: Option<String>,
    /// Agent type hint from the skill manifest.
    pub(crate) agent_type: Option<String>,
    /// Parent cancellation token — so Ctrl+C / stop propagates into sub-runs.
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Same resolver as the parent loop so `skill` tool calls during the SSE edge
    /// round resolve (nested skills run inline — sub-run has no `skill_executor`).
    pub(crate) skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// When set, headless tool-round status lines are forwarded through this
    /// channel instead of being silently dropped. The receiver (e.g. `/team run`)
    /// renders them with an agent-id prefix.
    pub(crate) progress_tx: Option<tokio::sync::mpsc::UnboundedSender<SubRunProgressEvent>>,
    /// Agent identifier used to tag progress events.
    pub(crate) agent_id: String,
    /// Fine-grained live stream for spawned-agent UI drill-in.
    pub(crate) stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Direct live stream sink for spawned agents; avoids buffering
    /// child output through an unbounded channel.
    pub(crate) stream_event_sink: Option<crate::cli::chat_stream::SharedStreamEventSink>,
    /// Cross-turn tool output cache for edge-path dedup within this sub-run.
    pub(crate) tool_cache: crate::cli::stream::stream_render::EdgeToolCache,
    /// Captured parent prefix, if the spawner resolved one. Consumed
    /// by `on_turn_completed` on the first successful ingested turn
    /// to emit a single [`ForkCacheEvent`]. `None` means the child
    /// wasn't asked to inherit — no probe runs.
    pub(crate) inherited_prefix: Option<astra_runtime::orchestration::InheritedChildPrefix>,
    /// Sink for fork-cache events. Shares lifetime with the
    /// executor. When `None` no probe fires — the executor simply
    /// didn't plumb one through (harmless, telemetry is off).
    pub(crate) fork_cache_sink:
        Option<std::sync::Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
    /// One-shot state tracking whether the first-turn probe has
    /// already fired. The hook is called every turn; we only want
    /// to emit one event per child spawn.
    pub(crate) fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState,
}

pub(crate) struct SubRunJournalIdentity {
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) next_item_seq: u64,
    /// Latest successfully appended conversational assistant item. Tool-call
    /// envelopes also use the assistant role, so only visible model content
    /// advances this acknowledgement identity.
    pub(crate) last_assistant_source_event_id: Option<String>,
    /// A failed canonical append stops the child at its next model boundary.
    /// Continuing to accumulate transcript data in memory while the journal
    /// is permanently unavailable is neither durable nor bounded.
    pub(crate) persistence_blocked: bool,
}

impl SubRunHost {
    /// Flush newly appended canonical prompt-history messages into the parent
    /// session journal. This is called after every successful model ingest and
    /// once more when the loop exits so interrupted tool rounds are retained.
    #[must_use]
    pub(crate) fn flush_agent_transcript(&mut self, state: &AgenticLoopState) -> bool {
        let (Some(journal), Some(identity)) = (&self.journal, &mut self.journal_identity) else {
            return true;
        };
        let captured = state.take_run_transcript_capture();
        let mut events = Vec::new();
        let mut accepted_messages = Vec::new();
        let mut assistant_source_event_id = None;
        let mut next_item_seq = identity.next_item_seq;
        for message in captured {
            let Some(role) = message.get("role").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if role == "system" {
                continue;
            }
            if let Some(event) = astra_services::session_journal::JournalEvent::transcript_item(
                &identity.session_id,
                &identity.run_id,
                &self.agent_id,
                next_item_seq,
                &message,
            ) {
                if role == "assistant" && assistant_message_has_visible_content(&message) {
                    assistant_source_event_id = event
                        .transcript_item
                        .as_ref()
                        .map(|item| item.source_event_id.clone());
                }
                next_item_seq = next_item_seq.saturating_add(1);
                events.push(event);
                accepted_messages.push(message);
            }
        }
        if events.is_empty() {
            return true;
        }
        match journal.append_bulk_no_sync(&events) {
            Ok(()) => {
                identity.next_item_seq = next_item_seq;
                identity.persistence_blocked = false;
                if assistant_source_event_id.is_some() {
                    identity.last_assistant_source_event_id = assistant_source_event_id;
                }
                true
            }
            Err(error) => {
                identity.persistence_blocked = true;
                state.restore_run_transcript_capture_front(accepted_messages);
                tracing::warn!(
                    %error,
                    session_id = %identity.session_id,
                    run_id = %identity.run_id,
                    count = events.len(),
                    "failed to append local child transcript; batch retained for retry"
                );
                false
            }
        }
    }

    /// Flush and fsync the local child transcript, returning the exact
    /// assistant item that proves a canonical reader has caught up to this
    /// durable boundary. No acknowledgement is produced for partial output
    /// that never became a canonical assistant message.
    pub(crate) async fn finalize_agent_transcript(
        &mut self,
        state: &AgenticLoopState,
    ) -> Option<String> {
        if !self.flush_agent_transcript(state) {
            return None;
        }
        let (Some(journal), Some(identity)) = (&self.journal, &self.journal_identity) else {
            return None;
        };
        let source_event_id = identity.last_assistant_source_event_id.clone()?;
        let journal = Arc::clone(journal);
        let sync_result = tokio::task::spawn_blocking(move || journal.sync_data()).await;
        if let Err(error) = sync_result
            .map_err(|error| format!("transcript fsync task failed: {error}"))
            .and_then(|result| result.map_err(|error| error.to_string()))
        {
            tracing::warn!(
                %error,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                "local child transcript fsync failed; live suffix remains authoritative"
            );
            return None;
        }
        Some(source_event_id)
    }

    fn persist_agent_transcript_evidence(
        &mut self,
        evidence: astra_turn_types::AgentTranscriptEvidence,
    ) {
        let (Some(journal), Some(identity)) = (&self.journal, &mut self.journal_identity) else {
            return;
        };
        let Some(event) = astra_services::session_journal::JournalEvent::transcript_evidence(
            &identity.session_id,
            &identity.run_id,
            &self.agent_id,
            identity.next_item_seq,
            &evidence,
        ) else {
            tracing::warn!(
                agent_id = %self.agent_id,
                run_id = %identity.run_id,
                "local agent transcript evidence was rejected"
            );
            return;
        };
        match journal.append_bulk_no_sync(&[event]) {
            Ok(()) => {
                identity.next_item_seq = identity.next_item_seq.saturating_add(1);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    session_id = %identity.session_id,
                    run_id = %identity.run_id,
                    "failed to append local child transcript evidence"
                );
            }
        }
    }
}

fn assistant_message_has_visible_content(message: &serde_json::Value) -> bool {
    ["content", "reasoning_content"].into_iter().any(|field| {
        message
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
    })
}

/// A progress event emitted by a sub-run agent.
#[derive(Debug, Clone)]
pub(crate) struct SubRunProgressEvent {
    pub agent_id: String,
    pub style: HeadlessStderrStyle,
    pub line: String,
}

pub(crate) fn persist_failed_subrun(state: &mut AgenticLoopState, error: &str) -> String {
    let failure_output = if state.final_text.trim().is_empty() {
        format!("[sub-run failed] {error}")
    } else {
        format!(
            "[sub-run failed] {error}\n\nPartial output:\n{}",
            state.final_text
        )
    };
    state.final_text = failure_output.clone();
    state.push_prompt_history_message(json!({
        "role": "assistant",
        "content": failure_output.clone(),
    }));
    state.step_recorder.end_turn(false);

    let summary = state.step_recorder.summary();
    let mut blocked_tools = state.restricted_tools.iter().cloned().collect::<Vec<_>>();
    blocked_tools.sort();
    if let Some(heavy) = state.step_recorder.build_heavy_checkpoint(
        &state.messages,
        state.max_turn_input_tokens,
        state.remaining_turns as u32,
        &blocked_tools,
        &state.recent_tools,
    ) {
        let checkpoint = astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy));
        let Some(user_id) = state.context_manifest_user_id.as_deref() else {
            tracing::warn!(
                session_id = %summary.session_id,
                "skipping failed subrun checkpoint because owner user_id is unavailable"
            );
            return failure_output;
        };
        let _ = astra_pipeline::step_checkpoint::write_step_checkpoint(
            user_id,
            &summary.session_id,
            summary.checkpoints,
            &checkpoint,
        );
    }

    failure_output
}

fn attach_runtime_volatile_injections(
    payload: &mut Value,
    injections: &[astra_runtime::turn::agentic_loop::host::VolatileInjection],
) {
    let Some(value) =
        astra_runtime::turn::agentic_loop::host::runtime_volatile_injections_edge_profile_value(
            injections,
        )
    else {
        return;
    };
    let Some(edge_profile) = payload
        .as_object_mut()
        .and_then(|root| root.get_mut("edge_profile"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    edge_profile.insert(
        astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS
            .to_string(),
        value,
    );
}

#[async_trait]
impl AgenticLoopHost for SubRunHost {
    fn memory_recall_scope(&self, _state: &AgenticLoopState) -> Option<(String, String)> {
        self.executor.memory_recall_scope()
    }

    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        if self
            .journal_identity
            .as_ref()
            .is_some_and(|identity| identity.persistence_blocked)
        {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ResourceLimit,
                "Child transcript persistence is unavailable; the sub-run stopped before accumulating non-durable conversation history.",
            ));
        }
        self.executor
            .set_send_message_context(state.messaging.mailbox.as_ref().map(|mailbox| {
                let run_id = state
                    .current_run_id
                    .clone()
                    .unwrap_or_else(|| mailbox.address.run_id.clone());
                crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                    agent_id: mailbox.address.agent_id.clone(),
                    run_id,
                    router: mailbox.router(),
                }
            }));

        // Drain runtime volatile as typed edge metadata. Do not splice it into
        // messages[]: that loses producer kind, pollutes prompt-facing history,
        // and makes soft runtime evidence look like user content.
        let runtime_volatile_injections = state.take_volatile_pending();

        let effective_model = self.model.as_deref();
        let effective_offering_id = self.offering_id.clone();
        let thinking = effective_model
            .map(|model| astra_turn_core::thinking_config::resolve_model_thinking(model).1)
            .unwrap_or_default();
        let interaction_mode = TurnInteractionMode::NonInteractive;
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        let mut effective_restricted_tools = state.restricted_tools.clone();
        effective_restricted_tools.extend(interaction_scoped_restrictions);
        if state.hooks.completion_settlement.text_only {
            effective_restricted_tools.extend(tool_names_from_schemas(&self.all_schemas));
        }

        let runtime_decision_user_intent = state.runtime_decision_user_intent();
        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: state.messages.as_slice(),
            user_intent: Some(runtime_decision_user_intent.as_str()),
            session_id: state.current_session_id.as_deref(),
            agent_id: Some(self.agent_id.as_str()),
            inference_purpose: state.inference_purpose,
            round_index: state.current_round_index,
            offering_id: Some(effective_offering_id.as_str()),
            interaction_mode: Some(interaction_mode.label()),
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "subrun",
            capabilities: astra_thin_client::builtin_capability_preset(),
            project_root: &self.project_root,
            git_branch: None,
            thinking: thinking.clone(),
        });

        attach_runtime_volatile_injections(&mut payload, &runtime_volatile_injections);

        if let Some(max_tokens) = self.max_completion_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        if let Some(ref effort) = self.effort {
            payload["effort"] = json!(effort);
        }

        if let Some(ref agent_type) = self.agent_type {
            payload["agent_type"] = json!(agent_type);
        }
        // Attach tool schemas. In fork mode, prefer the parent's frozen
        // canonical schemas so the tool-schema hash matches the parent's
        // cached prefix (cache key alignment). Falls back to live
        // registry if no frozen schemas are available.
        let base_tool_surface =
            astra_runtime::tool_registry::surface::ToolSurface::from_runtime_config(
                &self.all_schemas,
            );
        let schemas_to_use = resolve_subrun_schemas(
            self.inherited_prefix.as_ref(),
            base_tool_surface.always_load_schemas(),
        );
        state.last_turn_policy = attach_subrun_tool_surface(
            &mut payload,
            schemas_to_use,
            &self.all_schemas,
            &effective_restricted_tools,
            &self.executor,
            approximate_context_window_from_effective_input_budget(state.max_turn_input_tokens),
            interaction_mode,
        );

        set_payload_tool_results_if_non_empty(&mut payload, &state.tool_results);

        // Sub-runs share the parent's session_id but have no turn_event_buffer.
        // Tell the bridge not to write llm_round events — the parent's journal
        // already records delegation results. Without this, the bridge writes
        // duplicate rounds to the parent's journal file.
        if let Some(root) = payload.as_object_mut() {
            root.insert("root_turn_journal_owned".into(), json!(true));
        }

        let resp = self
            .api
            .post_chat_turn_retry_429(&self.token, &payload, 3, true)
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(astra_core::ClassifiedError::new(
                if status.as_u16() == 401 || status.as_u16() == 403 {
                    astra_core::ErrorKind::Auth
                } else if status.as_u16() == 429 {
                    astra_core::ErrorKind::RateLimit
                } else if status.is_server_error() {
                    astra_core::ErrorKind::ServerError
                } else {
                    astra_core::ErrorKind::Unknown
                },
                format!("Sub-run API error {status}: {body}"),
            ));
        }

        let edge_ctx = EdgeSseContext {
            api: &self.api,
            token: &self.token,
            executor_id: "subrun",
            executor: std::sync::Arc::clone(&self.executor),
            render_policy: RenderPolicy::Silent,
            perm_manager: Some(&mut self.perm_manager),
            cancel_token: self.cancel_token.as_ref().map(|t| t.as_ref()),
            stream_event_tx: self.stream_event_tx.clone(),
            stream_event_sink: self.stream_event_sink.clone(),
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut self.tool_cache,
            observability_hub: None,
            incremental_state: None,
        };

        let prep_line = ChatTurnPrepLineGuard::maybe_start(false, None);
        let turn = consume_turn_sse(
            prep_line,
            resp,
            false, // render_md
            80,    // term_width
            RenderPolicy::Silent,
            Some(edge_ctx),
            0,                                              // pre_clear_lines
            None,                                           // auth_profile
            self.cancel_token.as_ref().map(|t| t.as_ref()), // propagate parent cancel
        )
        .await;
        Ok(HostTurnResult {
            accum: turn.core,
            ttft_ms: turn.ttft_ms,
            edge_tool_round: turn.edge_tool_round,
            error_kind: None,
        })
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        if let Some(ref tx) = self.progress_tx {
            let _ = tx.send(SubRunProgressEvent {
                agent_id: self.agent_id.clone(),
                style,
                line,
            });
        }
    }

    fn on_agent_communication(&mut self, event: astra_messaging::AgentCommunicationEvent) {
        self.persist_agent_transcript_evidence(
            astra_turn_types::AgentTranscriptEvidence::AgentCommunication {
                event: event.clone(),
            },
        );
        let stream_event = crate::cli::chat_stream::StreamEvent::AgentCommunication(event);
        if let Some(sink) = self.stream_event_sink.as_ref() {
            sink.send(stream_event);
        } else if let Some(tx) = self.stream_event_tx.as_ref()
            && let Err(error) = tx.try_send(stream_event)
        {
            tracing::warn!(%error, agent_id = %self.agent_id, "sub-run stream event queue unavailable");
        }
    }

    fn is_quiet(&self) -> bool {
        self.progress_tx.is_none()
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tool_names
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        crate::cli::tool_surface_injection::install_injected_tool_schema(
            self.executor.as_ref(),
            schema,
            &mut self.all_schemas,
            &mut self.valid_tool_names,
            None,
        );
    }

    fn on_turn_completed(&mut self, state: &AgenticLoopState) {
        // PR 5.6: probe the first successful ingested turn's
        // cache_read_input_tokens against the parent-side estimate.
        if let Some(ref sink) = self.fork_cache_sink {
            astra_runtime::orchestration::maybe_emit_fork_cache_probe(
                &mut self.fork_cache_probe_state,
                self.inherited_prefix.as_ref(),
                state.current_run_id.as_deref().unwrap_or(""),
                state.total_cache_read,
                astra_turn_core::fork_cache_event::ForkCacheThresholds::default(),
                sink.as_ref(),
            );
        }

        if !self.flush_agent_transcript(state) {
            tracing::error!(
                target: "astra_cli::subrun_transcript",
                agent_id = %self.agent_id,
                "child transcript persistence failed; next model boundary will stop"
            );
        }

        // Unified timeline: emit the LATEST round event tagged with
        // agent_id to the parent's journal so the timeline renderer
        // can interleave child rounds with parent rounds.
        //
        // NOTE: `state.recent_rounds` is a **ring buffer** (capacity
        // RECENT_ROUNDS_RING_CAPACITY=32) that accumulates across
        // turns. Iterating the whole ring here would re-journal every
        // historical round on every turn end, causing duplicate
        // entries and inflated token accounting in the parent
        // timeline. Only the most recent entry — the round that just
        // completed — should be emitted.
        if let Some(ref journal) = self.journal {
            if let Some(round_summary) = state.recent_rounds.last() {
                let journal_session_id = self
                    .journal_identity
                    .as_ref()
                    .map(|identity| identity.session_id.as_str());
                let mut buf = astra_services::session_journal::TurnEventBuffer::begin_producer_turn(
                    journal_session_id,
                    state.current_round_index.saturating_add(1),
                );
                buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
                    purpose: round_summary.purpose,
                    duration_ms: round_summary.duration_ms,
                    prompt_tokens: round_summary.prompt_tokens,
                    completion_tokens: round_summary.completion_tokens,
                    cache_read_tokens: round_summary.cache_read_tokens,
                    cache_creation_tokens: round_summary.cache_creation_tokens,
                    tool_calls_returned: round_summary.tool_calls_returned,
                    tool_call_names: round_summary.tool_call_names.clone(),
                    finish_reason: round_summary.finish_reason.clone(),
                    source: Some("child_agent".to_string()),
                    run_id: self
                        .journal_identity
                        .as_ref()
                        .map(|identity| identity.run_id.clone()),
                    parent_run_id: self
                        .journal_identity
                        .as_ref()
                        .and_then(|identity| identity.parent_run_id.clone()),
                    agent_id: Some(self.agent_id.clone()),
                    ttft_ms: None,
                    agentic_step: None,
                    tool_calls: None,
                });
                let events = buf.drain();
                crate::cli::cli_config::cli_utils::append_bulk_journal_events_no_sync_or_warn(
                    journal,
                    journal_session_id,
                    &events,
                    "skill_subrun:flush_round_events",
                );
            }
        }
    }
}

// ─── CliSkillSubRunExecutor ──────────────────────────────────────────────────

/// CLI implementation of [`SkillSubRunExecutor`].
///
/// Creates a fresh [`SubRunHost`] and [`AgenticLoopState`] for each sub-run,
/// then runs [`run_agentic_loop_with_host`] to completion.
///
/// Inherits the parent session's full permission envelope so that fork
/// sub-runs enforce the same mode, rules, and session approvals as the parent.
pub(crate) struct CliSkillSubRunExecutor {
    api: astra_thin_client::ThinClient,
    token: String,
    default_model: Option<String>,
    project_root: PathBuf,
    /// Full permission envelope inherited from the parent session.
    inherited_permissions: astra_runtime::orchestration::InheritedPermissions,
    /// Parent cancellation token — propagated so Ctrl+C / stop interrupts subruns.
    cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Skill resolver inherited from parent — enables nested skill invocations.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Parent interactive session id for self-introspection persistence.
    active_session_id: Option<String>,
}

impl CliSkillSubRunExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        default_model: Option<String>,
        project_root: PathBuf,
        mut inherited_permissions: astra_runtime::orchestration::InheritedPermissions,
        cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        inherited_permissions.is_background = true;
        Self {
            api,
            token,
            default_model,
            project_root,
            inherited_permissions,
            cancel_token,
            skill_resolver: None,
            active_session_id: None,
        }
    }

    /// Attach a skill resolver so sub-runs can invoke other skills.
    pub fn with_skill_resolver(
        mut self,
        resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_active_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.active_session_id = Some(session_id.into());
        self
    }
}

#[async_trait]
impl SkillSubRunExecutor for CliSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        max_tokens: Option<u32>,
        allowed_tools: &[String],
        parent_recursion_depth: u8,
        effort: Option<&str>,
        agent_type: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                parent_recursion_depth,
            )?;
        let effective_model = self.default_model.clone();
        let model_selection =
            resolve_subrun_model_selection(&self.api, &self.token, effective_model.as_deref())
                .await?;
        let effective_model = Some(model_selection.name);
        let thinking = effective_model
            .as_deref()
            .map(|model| astra_turn_core::thinking_config::resolve_model_thinking(model).1)
            .unwrap_or_default();
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            effective_model.as_deref().unwrap_or(""),
        );
        // Resolve per-model workflow-guard policy up front; `effective_model`
        // is moved into the SubRunHost below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_policy
            .resolve_for_model(effective_model.as_deref());

        let all_schemas = edge_tools::local_tool_schemas();
        let valid_tool_names = tool_names_from_schemas(&all_schemas);
        let safe_name = astra_skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );

        // Issue #326 P5b: skill subruns are headless — never read
        // project allow rules. Deny rules and the user-level rule
        // file are still honoured (apply_load_policy(HeadlessSafe)).
        let perm_manager = PermissionManager::with_inherited(
            &self.project_root,
            self.inherited_permissions.clone(),
        );
        let permission_context = perm_manager.runtime_permission_handle();

        let executor = edge_tools::ToolExecutor::new(&self.project_root)
            .with_cloud(self.api.api_origin(), &self.token)
            .with_memory_attribution_id(subrun_session_id.clone());
        executor.set_cli_local_provider_schemas(all_schemas.clone());
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model.clone(),
            offering_id: model_selection.offering_id,
            project_root: self.project_root.clone(),
            executor: std::sync::Arc::new(executor),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: max_tokens,
            effort: effort.map(String::from),
            agent_type: agent_type.map(String::from),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
            // Skill sub-runs don't participate in fork-prefix cache
            // inheritance — skills are user-invoked, not spawner-
            // driven. Leave empty.
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
            journal_identity: None,
        };

        let messages = vec![
            json!({
                "role": "system",
                "content": instructions,
            }),
            json!({
                "role": "user",
                "content": if task_context.is_empty() {
                    format!("Execute the skill '{skill_name}' according to the instructions above.")
                } else {
                    task_context.to_string()
                },
            }),
        ];

        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != astra_runtime::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str()
                            != astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        let task_profile = infer_task_execution_profile(task_context);
        let user_id = cli_user_id();
        let step_recorder = StepRecorder::with_persistence(
            &user_id,
            &subrun_session_id,
            &format!("{}-task", subrun_session_id),
        );

        let mut state = AgenticLoopState {
            observation_store: None,
            observation_journal: Default::default(),
            messages,
            run_transcript_capture: None,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            session_memory_state: Default::default(),
            current_session_id: None,
            current_run_id: None,
            inference_purpose: astra_turn_types::InferencePurpose::SubAgent,
            context_manifest_pool: None,
            context_manifest_user_id: Some(user_id),
            context_manifest_model_name: effective_model.clone(),
            runtime_manifest: runtime_manifest_for_model(
                "cli_skill_subrun",
                "cli_skill_subrun",
                effective_model.as_deref(),
            ),
            recursion_depth: child_recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            final_output_ready_notified: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            last_finish_reason: None,
            total_observation_tool_calls: 0,
            has_any_usage: false,
            max_turns: SUBRUN_MAX_TURNS,
            remaining_turns: SUBRUN_MAX_TURNS,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: task_profile.agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::with_profile(task_profile),
            budget_policy: None,
            restricted_tools,
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                resolver: self.skill_resolver.clone(),
                quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(&self.project_root),
                session_event_hooks: astra_skills::hooks::load_session_event_hooks(
                    &self.project_root,
                ),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint: Some(self.project_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            messaging: Default::default(),
            user_intents: Default::default(),
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
            },
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    astra_runtime::turn::session_current_date::resolve_session_current_date(
                        self.active_session_id.as_deref().unwrap_or(""),
                    ),
                ),
            ),
            message: task_context.to_string(),
            user_intent: task_context.to_string(),
            recent_tools: Vec::new(),
            has_prior_assistant_turn: false,
            turn_intent: None,
            task_profile: infer_task_execution_profile(task_context),
            last_turn_policy: TurnInteractionPolicy::default(),
            api: self.api.clone(),
            api_token: self.token.clone(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "skill_subrun".to_string(),
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: SUBRUN_MAX_CUMULATIVE_TOKENS,
            thinking,
            recent_file_reads: Vec::new(),
            permission_context: Some(permission_context),
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            runtime_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: None,
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: astra_runtime::turn::harness_adapter::HarnessSlot::empty(),
        };

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
        let outcome = project_skill_subrun_outcome(&loop_result, &state);
        match &loop_result {
            Ok(AgenticLoopOutcome::Error(error)) => {
                persist_failed_subrun(&mut state, error);
            }
            Err(error) => {
                persist_failed_subrun(&mut state, &error.to_string());
            }
            _ => {}
        }

        let turns = (SUBRUN_MAX_TURNS - state.remaining_turns) as u32;
        let tokens_used = state.provider_total_tokens().min(u32::MAX as u64) as u32;

        Ok(SubRunResult {
            output: state.final_text,
            tokens_used,
            turns,
            outcome,
        })
    }
}

/// Resolve the tool schema set for a sub-run.
///
/// Returns the parent's frozen canonical schemas when fork-prefix
/// inheritance is active **and** schemas were captured; otherwise
/// returns `fallback_always_load` (the live surface's T1 set).
///
/// When fork inheritance is configured but `frozen_tool_schemas` is
/// `None` we **must** fall back, but we also emit a warning: the
/// resulting `tool_schema_hash` will not align with the parent's, so
/// the prefix-cache reuse path silently misses. Without telemetry the
/// regression looks like "cache just doesn't help today" and lingers
/// in production unnoticed (silent miss → wasted tokens). Loud is
/// better than silent.
fn resolve_subrun_schemas(
    inherited: Option<&astra_runtime::orchestration::InheritedChildPrefix>,
    fallback_always_load: Vec<Value>,
) -> Vec<Value> {
    match inherited {
        Some(ip) => match &ip.frozen_tool_schemas {
            Some(schemas) => schemas.clone(),
            None => {
                tracing::warn!(
                    target: "astra_cli::skill_subrun",
                    prefix_id = %ip.prefix_id,
                    parent_run_id = %ip.parent_run_id,
                    "fork inheritance active but frozen_tool_schemas is None; \
                     falling back to T1 always-load schemas — child tool_schema_hash \
                     will not match parent's, prefix-cache reuse will miss"
                );
                fallback_always_load
            }
        },
        None => fallback_always_load,
    }
}

fn empty_surface_report_for_schemas(
    schemas: &[Value],
) -> astra_turn_core::tool_registry_report::ToolSelectionReport {
    let mut visible_tools: Vec<String> = tool_names_from_schemas(schemas).into_iter().collect();
    visible_tools.sort();
    astra_turn_core::tool_registry_report::ToolSelectionReport {
        visible_count: visible_tools.len() as u32,
        visible_tools,
        schema_budget_used: 0,
        schema_budget_total: 0,
    }
}

// AgenticLoopState currently carries the effective input budget, not the full
// model context window. This approximation is used only for deferred-tool
// manifest sizing in sub-runs; callers with registry metadata should pass the
// full context window directly.
fn approximate_context_window_from_effective_input_budget(
    max_turn_input_tokens: u64,
) -> Option<u32> {
    if max_turn_input_tokens == 0 {
        return None;
    }
    let approx_context_window = max_turn_input_tokens.saturating_mul(10).div_ceil(8);
    Some(approx_context_window.min(u64::from(u32::MAX)) as u32)
}

fn attach_subrun_tool_surface(
    payload: &mut Value,
    mut schemas_to_use: Vec<Value>,
    all_schemas: &[Value],
    restricted_tools: &HashSet<String>,
    executor: &edge_tools::ToolExecutor,
    context_window_tokens: Option<u32>,
    interaction_mode: TurnInteractionMode,
) -> TurnInteractionPolicy {
    let activated = executor.activated_deferred_tool_names_for_schema_injection();
    let mut surface_report = empty_surface_report_for_schemas(&schemas_to_use);
    if !activated.is_empty() {
        let refs: Vec<&str> = activated.iter().map(String::as_str).collect();
        inject_required_tool_names(&mut schemas_to_use, &mut surface_report, &refs, all_schemas);
    }
    schemas_to_use = executor.runtime_bound_tool_schemas(schemas_to_use);

    astra_runtime::turn::agentic_prepare_payload::attach_filtered_edge_tools_to_payload(
        payload,
        schemas_to_use,
        restricted_tools,
    );
    let final_visible_schemas: Vec<Value> = payload
        .get("edge_tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let final_visible_tool_names =
        astra_turn_core::tool::schema::tool_names_from_schemas(&final_visible_schemas);
    let eligible_surface_schemas: Vec<Value> = all_schemas
        .iter()
        .filter(|schema| {
            schema
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .is_none_or(|name| !restricted_tools.contains(name))
        })
        .cloned()
        .collect();
    let eligible_surface_schemas = executor.runtime_bound_tool_schemas(eligible_surface_schemas);
    let eligible_provider_schemas =
        executor.runtime_bound_provider_owned_schemas_excluding(restricted_tools);
    let tool_surface = astra_runtime::tool_registry::surface::ToolSurface::build_excluding_visible(
        eligible_surface_schemas,
        &astra_config::runtime_config::RuntimeConfig::cached().tool_surface,
        &eligible_provider_schemas,
        &final_visible_tool_names,
    );
    let mut activatable_tool_names = HashSet::new();
    if final_visible_tool_names.contains("tool_search")
        && let Some(manifest) =
            tool_surface.deferred_manifest_with_context_window(context_window_tokens)
    {
        activatable_tool_names = manifest.names.iter().cloned().collect();
        merge_edge_profile_extensions(
            payload,
            &json!({
                astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT:
                    manifest.text,
                astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW:
                    manifest.context_window,
                astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES:
                    manifest.names,
                astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES:
                    manifest.omitted_names,
            }),
        );
    }
    // Mirror exactly what the sub-run request exposes. Visible and
    // activatable names are written together so tool_search/direct-call
    // recovery never observes a mixed surface.
    executor.set_current_tool_surface(&final_visible_schemas, activatable_tool_names);
    turn_policy_from_payload_edge_tools(payload, interaction_mode)
}

#[cfg(test)]
mod tests {
    use super::{
        CliSkillSubRunExecutor, SUBRUN_MAX_CUMULATIVE_TOKENS, SUBRUN_MAX_TURNS, SubRunHost,
        SubRunJournalIdentity, attach_runtime_volatile_injections, attach_subrun_tool_surface,
        resolve_subrun_schemas,
    };
    use astra_runtime::turn::agentic_loop::host::ASK_USER_TOOL_NAME;
    use astra_runtime::turn::agentic_loop::host::{
        AgenticLoopHost, TurnInteractionMode, interaction_scoped_tool_restrictions,
    };
    use astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES;
    use astra_skills::executor::isolated::SkillSubRunExecutor;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::path::PathBuf;

    use crate::cli::chat_stream::turn_policy_from_payload_edge_tools;
    use crate::cli::permission_manager::{PermissionManager, PermissionMode};
    use crate::edge_tools;

    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("{name} tool"),
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    fn subrun_host_with_journal(
        journal: std::sync::Arc<astra_services::session_journal::JournalWriter>,
        session_id: String,
        next_item_seq: u64,
        last_assistant_source_event_id: Option<String>,
    ) -> SubRunHost {
        let root = PathBuf::from(".");
        SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            offering_id: "offer-test".to_string(),
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            journal: Some(journal),
            journal_identity: Some(SubRunJournalIdentity {
                session_id,
                run_id: "child-run".into(),
                parent_run_id: Some("parent-run".into()),
                next_item_seq,
                last_assistant_source_event_id,
                persistence_blocked: false,
            }),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: "reviewer@test".into(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
        }
    }

    #[test]
    fn subrun_routes_runtime_feedback_as_typed_edge_metadata() {
        let mut payload = json!({
            "messages": [{"role": "user", "content": "continue child task"}],
            "edge_profile": {}
        });
        let injections = vec![astra_runtime::turn::agentic_loop::host::VolatileInjection {
            kind: astra_runtime::turn::agentic_loop::host::VolatileKind::PolicyAdvisory,
            payload: json!({"signal": "soft subrun evidence"}),
            round_index: 2,
        }];

        attach_runtime_volatile_injections(&mut payload, &injections);

        assert_eq!(
            payload["messages"],
            json!([{"role": "user", "content": "continue child task"}])
        );
        let lane = &payload["edge_profile"]
            [astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS];
        assert_eq!(lane[0]["kind"], "policy_advisory");
        assert_eq!(lane[0]["delivery_class"], "advisory_evidence");
        assert_eq!(lane[0]["payload"]["signal"], "soft subrun evidence");
        assert_eq!(lane[0]["round_index"], 2);
    }

    #[test]
    fn subrun_host_is_quiet_without_progress() {
        let root = PathBuf::from(".");
        let host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            offering_id: "offer-test".to_string(),
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
            journal_identity: None,
        };
        assert!(host.is_quiet());
    }

    #[test]
    fn subrun_host_not_quiet_with_progress() {
        let root = PathBuf::from(".");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            offering_id: "offer-test".to_string(),
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: Some(tx),
            agent_id: "test-agent".to_string(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
            journal_identity: None,
        };
        assert!(!host.is_quiet());
    }

    #[tokio::test]
    async fn failed_terminal_flush_never_acknowledges_an_older_assistant_item() {
        let session_id = format!("transcript-flush-failure-{}", uuid::Uuid::new_v4());
        let journal = std::sync::Arc::new(
            astra_services::session_journal::JournalWriter::new(&session_id).unwrap(),
        );
        let journal_path = journal.path().clone();
        std::fs::create_dir(journal.path()).unwrap();

        let mut host = subrun_host_with_journal(
            journal.clone(),
            session_id,
            7,
            Some("older-assistant-item".into()),
        );
        let mut state = astra_runtime::turn::agentic_loop::host::make_test_loop_state();
        let newest = json!({"role": "assistant", "content": "new result"});
        state.begin_run_transcript_capture([newest.clone()]);

        assert_eq!(host.finalize_agent_transcript(&state).await, None);
        assert_eq!(
            state.take_run_transcript_capture(),
            vec![newest],
            "failed append must remain retryable"
        );
        {
            let identity = host.journal_identity.as_ref().unwrap();
            assert_eq!(identity.next_item_seq, 7);
            assert!(
                identity.persistence_blocked,
                "a failed durable boundary must stop further transcript growth"
            );
            assert_eq!(
                identity.last_assistant_source_event_id.as_deref(),
                Some("older-assistant-item")
            );
        }

        drop(host);
        drop(journal);
        std::fs::remove_dir(journal_path).unwrap();
    }

    #[tokio::test]
    async fn terminal_flush_returns_identity_present_in_the_synced_local_journal() {
        let session_id = format!("transcript-flush-success-{}", uuid::Uuid::new_v4());
        let journal = std::sync::Arc::new(
            astra_services::session_journal::JournalWriter::new(&session_id).unwrap(),
        );
        let journal_path = journal.path().clone();
        let mut host = subrun_host_with_journal(journal.clone(), session_id.clone(), 1, None);
        let mut state = astra_runtime::turn::agentic_loop::host::make_test_loop_state();
        state.begin_run_transcript_capture([
            json!({"role": "user", "content": "review this"}),
            json!({
                "role": "assistant",
                "reasoning_content": "checked the invariant",
                "content": "the invariant holds"
            }),
        ]);

        let committed = host
            .finalize_agent_transcript(&state)
            .await
            .expect("assistant transcript identity");
        let events = astra_services::session_journal::read_journal(&session_id).unwrap();
        let committed_item = events
            .iter()
            .filter_map(|event| event.transcript_item.as_ref())
            .find(|item| item.source_event_id == committed)
            .expect("committed identity must resolve in canonical journal");
        assert_eq!(committed_item.message["content"], "the invariant holds");
        assert_eq!(
            committed_item.message["reasoning_content"],
            "checked the invariant"
        );
        assert!(state.take_run_transcript_capture().is_empty());

        drop(host);
        drop(journal);
        std::fs::remove_file(journal_path).unwrap();
    }

    #[test]
    fn subrun_host_inject_tool_schema_accepts_skill() {
        let root = PathBuf::from(".");
        let mut host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            offering_id: "offer-test".to_string(),
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
            journal_identity: None,
        };
        host.inject_tool_schema(astra_runtime::turn::skill_tool::skill_tool_schema_v2());
        assert!(host.valid_tool_names.contains("skill"));
        assert_eq!(host.all_schemas.len(), 1);
    }

    #[test]
    fn subrun_host_inject_tool_schema_rejects_unknown_tool() {
        let root = PathBuf::from(".");
        let mut host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            offering_id: "offer-test".to_string(),
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
            journal_identity: None,
        };
        host.inject_tool_schema(schema("not_registered"));
        assert!(host.valid_tool_names.is_empty());
        assert!(host.all_schemas.is_empty());
    }

    #[test]
    fn subrun_payload_policy_excludes_ask_user_in_noninteractive_mode() {
        let mut payload = json!({});
        let interaction_mode = TurnInteractionMode::NonInteractive;
        let mut restricted_tools = interaction_scoped_tool_restrictions(interaction_mode);

        astra_runtime::turn::agentic_prepare_payload::attach_filtered_edge_tools_to_payload(
            &mut payload,
            vec![schema("mo_query"), schema(ASK_USER_TOOL_NAME)],
            &mut restricted_tools,
        );

        let policy = turn_policy_from_payload_edge_tools(&payload, interaction_mode);
        assert_eq!(policy.visible_tool_names, vec!["mo_query".to_string()]);
        assert_eq!(policy.observation_tool_names, vec!["mo_query".to_string()]);
        assert!(!policy.allow_ask_user);
    }

    #[tokio::test]
    async fn subrun_surface_injects_activated_deferred_tool_and_excludes_it_from_manifest() {
        let root = tempfile::tempdir().unwrap();
        let executor = edge_tools::ToolExecutor::new(root.path());
        executor.set_current_visible_tool_schemas(&[schema("tool_search")]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let selected = executor
            .execute("tool_search", &json!({"query": "select:memory"}))
            .await;
        let selected: Value = serde_json::from_str(&selected).unwrap();
        assert_eq!(selected["matches"][0]["name"].as_str(), Some("memory"));
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()]
        );

        let mut payload = json!({});
        let mut restricted_tools = HashSet::new();
        let all_schemas = vec![schema("tool_search"), schema("memory"), schema("read_file")];

        let policy = attach_subrun_tool_surface(
            &mut payload,
            vec![schema("tool_search")],
            &all_schemas,
            &mut restricted_tools,
            &executor,
            Some(200_000),
            TurnInteractionMode::NonInteractive,
        );

        let visible_tool_names: HashSet<String> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .map(ToString::to_string)
            .collect();
        assert!(
            visible_tool_names.contains("memory"),
            "subrun must keep activated deferred tools in the next visible schema set: {visible_tool_names:?}"
        );
        assert!(policy.visible_tool_names.contains(&"memory".to_string()));

        let deferred_tool_names: HashSet<String> = payload["edge_profile"]
            [EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !deferred_tool_names.contains("memory"),
            "activated visible tool must not also remain in the deferred manifest: visible={visible_tool_names:?} deferred={deferred_tool_names:?}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "subrun surface assembly must not consume activation before the selected tool is called"
        );
        let _ = executor.execute("memory", &json!({})).await;
        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "the accepted visible tool call consumes the matching activation"
        );
    }

    #[test]
    fn subrun_surface_clears_stale_activatable_when_no_deferred_manifest() {
        let root = tempfile::tempdir().unwrap();
        let executor = edge_tools::ToolExecutor::new(root.path());
        executor.set_current_visible_tool_schemas(&[schema("tool_search")]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let mut payload = json!({});
        let mut restricted_tools = HashSet::new();
        let all_schemas = vec![schema("tool_search")];

        let policy = attach_subrun_tool_surface(
            &mut payload,
            vec![schema("tool_search")],
            &all_schemas,
            &mut restricted_tools,
            &executor,
            Some(200_000),
            TurnInteractionMode::NonInteractive,
        );

        assert_eq!(policy.visible_tool_names, vec!["tool_search".to_string()]);
        assert!(
            payload["edge_profile"][EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
                .as_array()
                .is_none_or(|names| names.is_empty()),
            "subrun payload without a deferred prompt block must not carry deferred names: {payload}"
        );
        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .is_empty(),
            "subrun must clear stale activatable names when no deferred manifest is rendered"
        );
    }

    #[tokio::test]
    async fn subrun_surface_does_not_advertise_unbound_deferred_runtime_tool() {
        let root = tempfile::tempdir().unwrap();
        let executor = edge_tools::ToolExecutor::new(root.path());

        let mut payload = json!({});
        let mut restricted_tools = HashSet::new();
        let all_schemas = vec![schema("tool_search"), schema("agent_fanout")];

        let policy = attach_subrun_tool_surface(
            &mut payload,
            vec![schema("tool_search")],
            &all_schemas,
            &mut restricted_tools,
            &executor,
            Some(200_000),
            TurnInteractionMode::NonInteractive,
        );

        assert_eq!(policy.visible_tool_names, vec!["tool_search".to_string()]);
        assert!(
            payload["edge_profile"][EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES]
                .as_array()
                .is_none_or(|names| names.is_empty()),
            "subrun payload must not advertise a deferred runtime tool that local tool_search cannot activate: {payload}"
        );
        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .is_empty(),
            "subrun executor activatable set must agree with the payload deferred manifest"
        );
        let search = executor
            .execute("tool_search", &json!({"query": "select:agent_fanout"}))
            .await;
        let search_json: Value = serde_json::from_str(&search).unwrap();
        assert!(
            search_json["matches"].as_array().unwrap().is_empty(),
            "subrun tool_search must not resolve unbound agent_fanout: {search_json}"
        );
    }

    #[test]
    fn subrun_surface_does_not_put_unbound_runtime_tool_in_tools_array() {
        let root = tempfile::tempdir().unwrap();
        let executor = edge_tools::ToolExecutor::new(root.path());

        let mut payload = json!({});
        let mut restricted_tools = HashSet::new();
        let all_schemas = vec![schema("tool_search"), schema("agent_fanout")];

        let policy = attach_subrun_tool_surface(
            &mut payload,
            vec![schema("tool_search"), schema("agent_fanout")],
            &all_schemas,
            &mut restricted_tools,
            &executor,
            Some(200_000),
            TurnInteractionMode::NonInteractive,
        );

        assert_eq!(policy.visible_tool_names, vec!["tool_search".to_string()]);
        let edge_tool_names: HashSet<String> = payload["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .map(ToString::to_string)
            .collect();
        assert!(
            !edge_tool_names.contains("agent_fanout"),
            "subrun tools[] must not advertise an unbound runtime tool: {payload}"
        );
    }

    #[tokio::test]
    async fn cli_skill_subrun_rejects_when_recursion_depth_limit_reached() {
        let inherited_permissions =
            astra_runtime::orchestration::InheritedPermissions::new(PermissionMode::Deny);
        let executor = CliSkillSubRunExecutor::new(
            astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            "token".to_string(),
            Some("test-model".to_string()),
            PathBuf::from("."),
            inherited_permissions,
            None,
        );
        let allowed_tools: Vec<String> = Vec::new();

        let err = executor
            .execute_skill_subrun(
                "depth-test",
                "Do work",
                "task",
                None,
                &allowed_tools,
                astra_turn_core::agentic_recursion_guard::ABSOLUTE_MAX_AGENT_RECURSION_DEPTH,
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(err.contains("recursion depth 8 reached absolute safety ceiling 8"));
    }

    #[test]
    fn cli_skill_subrun_constructor_marks_permissions_background() {
        let inherited_permissions =
            astra_runtime::orchestration::InheritedPermissions::new(PermissionMode::Auto);
        let executor = CliSkillSubRunExecutor::new(
            astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            "token".to_string(),
            None,
            PathBuf::from("."),
            inherited_permissions,
            None,
        );

        assert_eq!(
            executor.inherited_permissions.mode,
            astra_runtime::orchestration::PermissionMode::Auto
        );
        assert!(executor.inherited_permissions.is_background);
    }

    // ── Phase-R10 adversarial contract guards (CLI-side constants) ───────
    //
    // These assert the exact values of [`SUBRUN_MAX_TURNS`] and
    // [`SUBRUN_MAX_CUMULATIVE_TOKENS`] so silent drift (e.g. a typo
    // bumping 25→35 or 120_000→12_000) breaks this test loudly.
    // The server-side equivalents are covered in
    // `crates/astra-cli/tests/phase_r10_skill_subrun_contracts.rs`
    // via the now-`pub` constants in
    // [`astra_runtime::server::server_skill_subrun`].

    #[test]
    fn cli_subrun_max_turns_is_exactly_25() {
        assert_eq!(SUBRUN_MAX_TURNS, 25);
    }

    #[test]
    fn cli_subrun_max_cumulative_tokens_is_exactly_120_000() {
        assert_eq!(SUBRUN_MAX_CUMULATIVE_TOKENS, 120_000);
    }

    /// No fork inheritance → just use the live surface's always-load set.
    #[test]
    fn resolve_subrun_schemas_no_inheritance_uses_always_load_fallback() {
        let always_load = vec![schema("read_file"), schema("write_file")];
        let resolved = resolve_subrun_schemas(None, always_load.clone());
        assert_eq!(resolved, always_load);
    }

    /// Fork inheritance with captured schemas → use the captured set
    /// verbatim so the child's tool_schema_hash matches the parent's.
    #[test]
    fn resolve_subrun_schemas_fork_with_frozen_uses_parent_schemas() {
        use astra_runtime::orchestration::InheritedChildPrefix;
        let frozen = vec![schema("bash"), schema("grep")];
        let always_load_fallback = vec![schema("read_file"), schema("write_file")];
        let ip = InheritedChildPrefix {
            prefix_id: "p1".into(),
            parent_run_id: "r1".into(),
            provider: astra_turn_core::fork_prefix::ProviderKind::Anthropic,
            thinking: None,
            prefix_messages: Vec::new(),
            frozen_tool_schemas: Some(frozen.clone()),
            expected_cache_read_tokens: 0,
        };
        let resolved = resolve_subrun_schemas(Some(&ip), always_load_fallback);
        assert_eq!(resolved, frozen);
    }

    /// Fork inheritance present but `frozen_tool_schemas` is None — the
    /// degenerate case the reviewer flagged. We still have to return
    /// *something* that lets the child run, so we fall back to the
    /// T1 always-load set, but the helper's job is to make the regression
    /// loud (verified by the tracing target/log assertions in the
    /// surrounding integration; here we pin behavior + payload shape).
    #[test]
    fn resolve_subrun_schemas_fork_without_frozen_falls_back_to_always_load() {
        use astra_runtime::orchestration::InheritedChildPrefix;
        let always_load_fallback = vec![schema("read_file"), schema("write_file")];
        let ip = InheritedChildPrefix {
            prefix_id: "p2".into(),
            parent_run_id: "r2".into(),
            provider: astra_turn_core::fork_prefix::ProviderKind::Anthropic,
            thinking: None,
            prefix_messages: Vec::new(),
            frozen_tool_schemas: None,
            expected_cache_read_tokens: 0,
        };
        let resolved = resolve_subrun_schemas(Some(&ip), always_load_fallback.clone());
        // Behaviour: must return fallback (NOT empty, NOT inherited).
        assert_eq!(resolved, always_load_fallback);
    }
}
