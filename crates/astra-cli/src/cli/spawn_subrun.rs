//! CLI implementation of SpawnAgentExecutor.
//!
//! Runs spawned agents using the same agentic loop infrastructure as delegation.

use astra_server_types::{ModelAdmissionRequestV1, ModelAdmissionSlotV1};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use astra_pipeline::step_recorder::StepRecorder;
use astra_runtime::{
    orchestration::{
        CancellationOrigin, InheritedPermissions, PermissionSummary, PreparedSpawn,
        PreparedSpawnModelIdentity, SpawnAgentExecutor, SpawnAgentInput, SpawnContext,
        SpawnRunConfig, SpawnRunResult, project_subrun_status_to_spawn,
        spawn_completion_status_from_finish_reason,
    },
    turn::agentic_loop::finalization::run_agentic_loop_with_host,
    turn::agentic_loop::host::{
        AgenticLoopOutcome, AgenticLoopState, CancellationState, MessagingState, SkillState,
        StopHookState, runtime_manifest_for_model,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::turn_guard::TurnGuard,
};
use astra_turn_core::{
    agent_live_event::SharedAgentLiveEventSink, interruption::InterruptionKind,
    orchestration_fanout_group::AgentFanoutSlotIdentity, tool::schema::tool_names_from_schemas,
};
use serde_json::{Value, json};

use super::chat_stream::StreamEvent;
use super::skill_subrun::{SubRunHost, SubRunJournalIdentity};
use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::edge_tools;

// ─── CliSpawnAgentExecutor ──────────────────────────────────────────────────

/// Re-export from runtime so all CLI components share one type.
pub type TokenProvider = astra_runtime::capabilities::TokenProvider;

/// Build the shared skill state used by local child conversations.
pub(crate) fn build_child_skill_state(
    request_constraints: astra_runtime::turn::agentic_loop::host::RequestConstraints,
    resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    effective_root: &Path,
) -> SkillState {
    SkillState {
        request_constraints,
        resolver,
        quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
        improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
        tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(effective_root),
        session_event_hooks: astra_skills::hooks::load_session_event_hooks(effective_root),
        ..Default::default()
    }
}

/// Restrict the admitted tool surface using an explicit allowlist.
pub(crate) fn build_restricted_tools(
    allow_tools: Option<&[String]>,
    valid_tool_names: &HashSet<String>,
) -> HashSet<String> {
    let Some(allow_tools) = allow_tools else {
        return HashSet::new();
    };
    let allowed: HashSet<&str> = allow_tools.iter().map(String::as_str).collect();
    valid_tool_names
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .cloned()
        .collect()
}

fn cancelled_loop_origin(interruption_kind: Option<InterruptionKind>) -> CancellationOrigin {
    if interruption_kind == Some(InterruptionKind::UserCancelled) {
        CancellationOrigin::User
    } else {
        CancellationOrigin::Runtime
    }
}

fn classified_error_cancellation_origin(
    error_kind: astra_core::ErrorKind,
) -> Option<CancellationOrigin> {
    (error_kind == astra_core::ErrorKind::Cancelled).then_some(CancellationOrigin::Runtime)
}

#[derive(Default)]
struct SessionTranscriptBinding {
    active_session_id: Option<String>,
    journal: Option<std::sync::Arc<astra_services::session_journal::JournalWriter>>,
}

/// CLI implementation of [`SpawnAgentExecutor`].
///
/// Runs spawned agents using the same agentic loop as delegation,
/// but with agent-type-specific configuration (model, tools, prompts).
pub struct CliSpawnAgentExecutor {
    api: astra_thin_client::ThinClient,
    /// Token captured at construction. Used as the **fallback** when
    /// `token_provider` is unset OR returns `None`. In production the
    /// REPL installs a provider so sub-agent spawns always read the
    /// freshest token; this field stays as a safety net for tests and
    /// the one-shot `chat -m` path that doesn't have a profile to query.
    token: String,
    /// Reads the current access token at spawn time. When set, takes
    /// precedence over `self.token` so token refreshes done by the
    /// parent turn flow propagate to children. When `None`, the executor
    /// falls back to `self.token` for parity with the pre-fix behaviour.
    token_provider: Option<TokenProvider>,
    project_root: PathBuf,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Parent session transcript binding. This can arrive after executor
    /// construction when the first streamed `session_info` event names a new
    /// session.
    session_transcript: std::sync::Mutex<SessionTranscriptBinding>,
    /// Optional sink for fork-cache telemetry. When `None` the
    /// executor still forwards `inherited_prefix` so child messages
    /// prepend the parent prefix — but no ForkCacheEvent is emitted.
    /// Zero-cost when unset.
    fork_cache_sink: Option<Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
    /// Shared command queue for the parent's BackgroundTaskRegistry.
    /// Threaded into the child's ToolExecutor so spawned sub-agents
    /// can inspect or stop background shell tasks promoted in the TUI.
    bg_task_commands: Option<Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>>,
    /// Threaded from `SessionState.bg_task_list_cache` so spawned
    /// sub-agents can read the latest task-list snapshot directly.
    bg_task_list_cache: Option<std::sync::Arc<tokio::sync::RwLock<String>>>,
    /// Session/default model fallback when the spawn request itself omits one.
    default_model: Option<String>,
    executions: std::sync::Mutex<HashMap<(String, String), Arc<CliSpawnExecution>>>,
}

/// Build the child agent's message array from system prompt, optional
/// inherited prefix, and the child task. Ensures role alternation is
/// valid for providers that require strict user/assistant alternation
/// (e.g. Bedrock Converse).
///
/// **Fork mode** (`prefix_messages` is `Some`): The prefix already
/// contains the parent's system message at [0] plus its full
/// conversation history — reconstructed byte-for-byte from the
/// captured `ForkPrefix.canonical_prefix_bytes`. We do NOT prepend a
/// fresh system message because that would:
/// - Create a duplicate system message (providers only expect one)
/// - Break byte-for-byte prefix cache reuse (the extra bytes shift
///   the cache key so the parent's cached KV is unusable)
///
/// The child's runtime identity is carried by typed context; the system block
/// contains only the reusable sub-agent role and execution contract.  This
/// keeps fresh sibling children on one provider-cache prefix.
///
/// **Fresh mode** (`prefix_messages` is `None`): the child gets the reusable
/// system role/contract, then the child task as a user message.
pub(crate) fn build_child_messages(
    system_prompt: &str,
    prefix_messages: Option<&[Value]>,
    child_task: &str,
    force_reasoning_field: bool,
    turn_chain_id: &str,
) -> Vec<Value> {
    fn ensure_assistant_reasoning_fields(messages: &mut [Value]) {
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) == Some("assistant")
                && msg.get("reasoning_content").is_none()
            {
                msg["reasoning_content"] = Value::String(String::new());
            }
        }
    }

    if let Some(prefix) = prefix_messages {
        // Fork mode: reuse parent prefix verbatim for cache alignment.
        let mut messages = Vec::with_capacity(prefix.len() + 2);
        crate::cli::history_work::record_json_history(
            astra_core::history_work::HistoryWorkSite::CliForkChildHistoryMaterialization,
            prefix,
        );
        messages.extend(prefix.iter().cloned());
        if force_reasoning_field {
            ensure_assistant_reasoning_fields(&mut messages);
        }
        // Bedrock Converse requires strict role alternation. If the
        // prefix ends with user or tool role, inserting the child task
        // (also user) would create consecutive user messages → HTTP 400.
        // Insert a synthetic assistant bridge to maintain alternation.
        let last_role = messages
            .iter()
            .rev()
            .find_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system");
        if matches!(last_role, Some("user") | Some("tool")) {
            let mut bridge = json!({
                "role": "assistant",
                "content": "I'll now work on the delegated task."
            });
            if force_reasoning_field {
                bridge["reasoning_content"] = Value::String(String::new());
            }
            messages.push(bridge);
        }
        let mut current_task = json!({ "role": "user", "content": child_task });
        astra_turn_types::mark_turn_message(&mut current_task, turn_chain_id);
        messages.push(current_task);
        messages
    } else {
        // Fresh mode: system prompt + child task only.
        let mut messages = vec![
            json!({ "role": "system", "content": system_prompt }),
            json!({ "role": "user", "content": child_task }),
        ];
        astra_turn_types::mark_turn_message(
            messages
                .last_mut()
                .expect("fresh child messages always contain the task"),
            turn_chain_id,
        );
        messages
    }
}

/// Build the reusable fresh-child system prompt.  Per-run IDs stay in typed
/// runtime context so sibling children can share this provider-cache prefix.
fn build_child_system_prompt(system_prompt_addendum: &str) -> String {
    if system_prompt_addendum.is_empty() {
        "You are a specialized sub-agent. Complete the task thoroughly.".to_string()
    } else {
        format!(
            "You are a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
            system_prompt_addendum
        )
    }
}

#[derive(Clone)]
struct AgentLiveStreamEventSink {
    run_id: String,
    agent_id: String,
    sink: SharedAgentLiveEventSink,
}

impl std::fmt::Debug for AgentLiveStreamEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLiveStreamEventSink")
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl super::chat_stream::StreamEventSink for AgentLiveStreamEventSink {
    fn send(&self, event: StreamEvent) {
        if let StreamEvent::AgentLiveGap(gap) = event {
            if let Err(err) = self.sink.send_gap(gap) {
                astra_core::agent_warn!(
                    "spawn_subrun",
                    "failed to forward live-gap notice for {}: {err:?}",
                    self.agent_id
                );
            }
            return;
        }
        let event = match event {
            StreamEvent::AgentLive(event) => event,
            event => {
                let Some(kind) = stream_event_to_agent_live_kind(event) else {
                    return;
                };
                astra_turn_core::agent_live_event::AgentLiveEvent {
                    run_id: self.run_id.clone(),
                    agent_id: self.agent_id.clone(),
                    kind,
                }
            }
        };
        if let Err(err) = self.sink.send(event) {
            astra_core::agent_warn!(
                "spawn_subrun",
                "dropping live event for {}: {err:?}",
                self.agent_id
            );
        }
    }
}

pub(crate) fn agent_live_stream_event_sink(
    run_id: String,
    agent_id: String,
    sink: Option<SharedAgentLiveEventSink>,
) -> Option<super::chat_stream::SharedStreamEventSink> {
    Some(Arc::new(AgentLiveStreamEventSink {
        run_id,
        agent_id,
        sink: sink?,
    }))
}

pub(crate) fn emit_agent_terminated(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    started_at: std::time::Instant,
    termination: astra_turn_core::agent_live_event::AgentLiveTermination,
    reason: Option<String>,
) {
    use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
    let Some(sink) = sink else {
        return;
    };
    if let Err(err) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms: started_at.elapsed().as_millis() as u64,
            reason,
        },
    }) {
        astra_core::agent_warn!(
            "spawn_subrun",
            "failed to emit terminal live event for {agent_id}: {err:?}"
        );
    }
}

pub(crate) fn emit_agent_execution_waiting(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    reason: String,
) {
    use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal};
    let Some(sink) = sink else {
        return;
    };
    if let Err(err) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting { reason }),
    }) {
        astra_core::agent_warn!(
            "spawn_subrun",
            "dropping waiting live event for {agent_id}: {err:?}"
        );
    }
}

pub(crate) fn emit_agent_transcript_committed(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    source_event_id: String,
    transcript_location: astra_turn_types::AgentTranscriptLocation,
) {
    use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal};
    let Some(sink) = sink else {
        return;
    };
    if let Err(error) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted {
            model_item_id: None,
            source_event_id,
            transcript_location,
        }),
    }) {
        tracing::debug!(
            %run_id,
            %agent_id,
            ?error,
            "canonical transcript commit was not delivered to the live workbench"
        );
    }
}

fn stream_event_to_agent_live_kind(
    event: StreamEvent,
) -> Option<astra_turn_core::agent_live_event::AgentLiveEventKind> {
    use astra_turn_core::agent_live_event::{AgentLiveEventKind, AgentLiveSignal};
    match event {
        StreamEvent::Token {
            model_item_id,
            text,
        } => Some(AgentLiveEventKind::OutputDelta {
            model_item_id,
            text,
        }),
        StreamEvent::ThinkingChunk {
            model_item_id,
            text,
        } => Some(AgentLiveEventKind::ThinkingDelta {
            model_item_id,
            text,
        }),
        StreamEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            ..
        } => Some(AgentLiveEventKind::ToolStarted {
            name,
            description,
            tool_use_id,
        }),
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            ..
        } => Some(AgentLiveEventKind::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
        }),
        StreamEvent::WaitingForModel => {
            Some(AgentLiveEventKind::Signal(AgentLiveSignal::WaitingForModel))
        }
        StreamEvent::ModelResponding => {
            Some(AgentLiveEventKind::Signal(AgentLiveSignal::ModelResponding))
        }
        StreamEvent::AssistantOutputSettled => {
            Some(AgentLiveEventKind::Signal(AgentLiveSignal::OutputSettled))
        }
        StreamEvent::RunInterrupted { user_message } => {
            Some(AgentLiveEventKind::Status { text: user_message })
        }
        StreamEvent::AskUserPrompted { request_id, prompt } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::AskUserPrompted { request_id, prompt },
        )),
        StreamEvent::AskUserResolved {
            request_id,
            resolution,
        } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::AskUserResolved {
                request_id,
                resolution,
            },
        )),
        StreamEvent::StatusLine(text) => Some(AgentLiveEventKind::Status { text }),
        StreamEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::UserIntentApplied {
                intent_id,
                delivery,
                status,
                event_index,
                content,
            },
        )),
        StreamEvent::UserIntentReturned {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::UserIntentReturned {
                intent_id,
                delivery,
                status,
                event_index,
                content,
            },
        )),
        StreamEvent::AgentCommunication(event) => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::AgentCommunication(event),
        )),
        StreamEvent::PermissionAutoApproved { tool, reason } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::PermissionAutoApproved { tool, reason },
        )),
        StreamEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            ..
        } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::AgentControlStarted {
                action,
                label,
                tool_use_id,
            },
        )),
        StreamEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        } => Some(AgentLiveEventKind::Signal(
            AgentLiveSignal::AgentControlCompleted {
                action,
                label,
                status,
                duration_ms,
                output,
                tool_use_id,
                agent_id,
            },
        )),
        StreamEvent::ToolOutput { name, lines, bytes } => {
            Some(AgentLiveEventKind::Signal(AgentLiveSignal::ToolProgress {
                name,
                lines,
                bytes,
            }))
        }
        StreamEvent::SessionBound(_)
        | StreamEvent::RunBound(_)
        | StreamEvent::ContextWindowPolicy { .. }
        | StreamEvent::ContextWindowEstimated(_)
        | StreamEvent::ContextSystemPromptTokens(_)
        | StreamEvent::ContextWindowMeasured(_)
        | StreamEvent::RequestTokenUsage(_)
        | StreamEvent::RuntimeFeedback(_)
        | StreamEvent::Thinking(_)
        | StreamEvent::AgentLive(_)
        | StreamEvent::AgentLiveGap(_)
        | StreamEvent::Compaction(_)
        | StreamEvent::ExplainAnalyze(_)
        | StreamEvent::ExplainAnalyzeSnapshot { .. }
        | StreamEvent::ArtifactPublication(_)
        | StreamEvent::ExplainAnalyzeGap
        | StreamEvent::WorkTaskBoardUpdate(_)
        | StreamEvent::VerdictReport(_) => None,
    }
}

impl CliSpawnAgentExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        project_root: PathBuf,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        Self {
            api,
            token,
            token_provider: None,
            project_root,
            cancel_token,
            skill_resolver: None,
            session_transcript: std::sync::Mutex::new(SessionTranscriptBinding::default()),
            fork_cache_sink: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            default_model: None,
            executions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    /// Install a token provider so each spawn reads the freshest
    /// access token at the moment of execution. The REPL wires this
    /// to `current_access_token(profile)` so token refreshes done by
    /// the parent agent's 401-retry path propagate to sub-agents.
    /// Without this, long-running sessions hit "Could not validate
    /// credentials" on every spawn after the first token rotation.
    pub fn with_token_provider(mut self, provider: TokenProvider) -> Self {
        self.token_provider = Some(provider);
        self
    }

    async fn resolve_token_async(&self) -> Result<String, String> {
        let fallback = self.token.clone();
        let Some(provider) = self.token_provider.clone() else {
            return Ok(fallback);
        };
        tokio::task::spawn_blocking(move || provider().unwrap_or(fallback.clone()))
            .await
            .map_err(|err| format!("token provider task failed: {err}"))
    }

    /// Install the parent's bg task command queue so spawned children
    /// can inspect or stop background shell tasks.
    pub(crate) fn with_bg_task_commands(
        mut self,
        commands: Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>,
    ) -> Self {
        self.bg_task_commands = Some(commands);
        self
    }

    /// Install the parent's bg task list cache so spawned children
    /// can read the latest task-list snapshot directly.
    pub fn with_bg_task_list_cache(
        mut self,
        cache: std::sync::Arc<tokio::sync::RwLock<String>>,
    ) -> Self {
        self.bg_task_list_cache = Some(cache);
        self
    }

    /// Bind this executor to one canonical local session transcript.
    ///
    /// A spawned agent is a run inside that session, so its journal writer and
    /// active session identity must be installed together. Keeping them as one
    /// operation prevents a live agent row whose run can never be reopened as
    /// a transcript. Journal availability is an observability degradation, not
    /// a reason to reject useful agent work: the executor keeps its session
    /// identity for tools and logs the persistence failure.
    pub(crate) fn with_session_transcript(self, session_id: impl Into<String>) -> Self {
        self.bind_session_transcript(session_id.into());
        self
    }

    fn session_transcript_snapshot(
        &self,
    ) -> (
        Option<String>,
        Option<std::sync::Arc<astra_services::session_journal::JournalWriter>>,
    ) {
        let guard = self
            .session_transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (guard.active_session_id.clone(), guard.journal.clone())
    }

    fn bind_session_transcript(&self, session_id: String) {
        if session_id.trim().is_empty() {
            return;
        }
        {
            let guard = self
                .session_transcript
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.active_session_id.as_deref() == Some(session_id.as_str()) {
                return;
            }
        }

        let journal = match astra_services::session_journal::JournalWriter::new(&session_id) {
            Ok(writer) => Some(Arc::new(writer)),
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "local spawned-agent transcript journal is unavailable"
                );
                None
            }
        };

        let mut guard = self
            .session_transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.active_session_id = Some(session_id);
        guard.journal = journal;
    }

    /// Install a fork-cache event sink. When present, every child
    /// spawn that inherited a parent prefix emits exactly one
    /// `ForkCacheEvent` on its first ingested turn.
    pub fn with_fork_cache_sink(
        mut self,
        sink: Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>,
    ) -> Self {
        self.fork_cache_sink = Some(sink);
        self
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }
}

#[async_trait]
impl SpawnAgentExecutor for CliSpawnAgentExecutor {
    async fn cancel_spawned_run_durably(
        &self,
        run_id: &str,
        binding: Option<&str>,
        _user: Option<&str>,
        _reason: &str,
        origin: CancellationOrigin,
    ) -> Result<astra_runtime::orchestration::SpawnRunCancellationDurability, String> {
        let binding = binding.ok_or("CLI cancellation requires an exact invocation binding")?;
        let control = astra_core::sync_poison::recover_mutex_lock(&self.executions)
            .get(&(run_id.to_owned(), binding.to_owned()))
            .cloned();
        if let Some(control) = control {
            {
                let mut current = astra_core::sync_poison::recover_mutex_lock(&control.origin);
                if origin == CancellationOrigin::User || *current != CancellationOrigin::User {
                    *current = origin;
                }
            }
            control.cancellation.cancel();
            // The receipt exists only after provision, execution, and cleanup
            // workers actually finish. Observer timeout never cancels that owner.
            let _ = control.wait().await;
        }
        Ok(astra_runtime::orchestration::SpawnRunCancellationDurability::LocalExecution)
    }

    fn bind_parent_session(&self, session_id: &str) {
        self.bind_session_transcript(session_id.to_string());
    }

    async fn prepare_batch(
        self: Arc<Self>,
        inputs: &[SpawnAgentInput],
        context: &SpawnContext,
        parent_selection: Option<&astra_turn_types::ModelSelection>,
    ) -> Result<Vec<Box<dyn PreparedSpawn>>, String> {
        use astra_turn_core::orchestration_spawn_tool::{
            ReasoningSelection, resolve_child_thinking,
        };

        for input in inputs {
            input.fanout_slot_identity()?;
            if let (Some(reasoning), Some(limit)) =
                (input.reasoning.as_ref(), input.max_output_tokens)
            {
                reasoning
                    .config()
                    .validate_output_budget(u64::from(limit))?;
            }
        }
        let parent_selection = parent_selection.or_else(|| {
            context
                .parent_model_reasoning
                .as_ref()
                .map(|parent| &parent.selection)
        });
        let requested_selectors: Vec<_> = inputs
            .iter()
            .map(|input| {
                let selector = astra_runtime::orchestration::selector_for_admitted_spawn_input(
                    input,
                    parent_selection,
                )
                .map_err(|error| error.to_string())?;
                if let (
                    Some(prepared),
                    Some(astra_turn_types::ModelSelector::OfferingId { offering_id }),
                ) = (input.resolved_model_selection.as_ref(), selector.as_ref())
                    && prepared.offering_id != *offering_id
                {
                    return Err(
                        "resolved child Offering does not match its requested model policy".into(),
                    );
                }
                Ok(selector)
            })
            .collect::<Result<_, String>>()?;
        let resolved_selections: Vec<_> = requested_selectors
            .iter()
            .map(|selector| match selector.as_ref() {
                Some(astra_turn_types::ModelSelector::OfferingId { offering_id }) => {
                    Some(astra_turn_types::ModelSelection {
                        offering_id: offering_id.clone(),
                    })
                }
                Some(astra_turn_types::ModelSelector::ConfiguredName { .. }) => None,
                None => None,
            })
            .collect();
        let mut thinking: Vec<_> = inputs
            .iter()
            .zip(&requested_selectors)
            .zip(&resolved_selections)
            .map(|((input, selector), selection)| {
                let configured_name = matches!(
                    selector,
                    Some(astra_turn_types::ModelSelector::ConfiguredName { .. })
                );
                resolve_child_thinking(
                    input.reasoning.as_ref(),
                    selection
                        .as_ref()
                        .or_else(|| (!configured_name).then_some(parent_selection).flatten()),
                    (!configured_name)
                        .then_some(context.parent_model_reasoning.as_ref())
                        .flatten(),
                )
            })
            .collect();
        let has_override = inputs
            .iter()
            .zip(&requested_selectors)
            .zip(&resolved_selections)
            .any(|((input, selector), selection)| {
                selector.as_ref().is_some_and(|selector| match selector {
                    astra_turn_types::ModelSelector::ConfiguredName { .. } => true,
                    astra_turn_types::ModelSelector::OfferingId { offering_id } => {
                        parent_selection.is_none_or(|parent| offering_id != &parent.offering_id)
                    }
                }) || (selector.is_none() && selection.is_some() && parent_selection.is_none())
                    || input.reasoning.is_some()
                    || input.max_output_tokens.is_some()
            });
        if has_override
            && parent_selection.is_none()
            && inputs.iter().any(|input| {
                matches!(
                    input.requested_model_policy,
                    None | Some(astra_turn_types::RequestedModelPolicy::Inherit)
                )
            })
        {
            return Err("a fanout with per-slot overrides requires an exact parent Offering for inherited slots".to_string());
        }
        // Reuse the exact parent execution snapshot for ordinary inherited
        // fanout. The parent already used this Offering, and Server rechecks
        // authorization at every child provider request; another catalog scan
        // or admission query here would add DB work without granting authority.
        // New Offerings and explicit per-slot constraints still use one
        // all-or-error batch admission before any child starts.
        let parent_model_snapshot = parent_selection
            .zip(context.resolved_model_name.as_deref())
            .filter(|(_, name)| !name.trim().is_empty());
        let batch_admission =
            has_override || (parent_selection.is_some() && parent_model_snapshot.is_none());
        let needs_token = batch_admission || parent_selection.is_none();
        let token = if needs_token {
            Some(self.resolve_token_async().await?)
        } else {
            None
        };
        let model_provenance = if batch_admission {
            "admission_validated"
        } else if parent_selection.is_some() {
            "inherited_parent_context"
        } else {
            "catalog_resolved"
        };
        let selections = if batch_admission {
            let mut slots = Vec::with_capacity(inputs.len());
            let mut slot_indexes = Vec::with_capacity(inputs.len());
            let mut distinct_slots = HashMap::<String, usize>::new();
            for ((input, selector), child_thinking) in
                inputs.iter().zip(&requested_selectors).zip(&thinking)
            {
                let selector = selector
                    .clone()
                    .or_else(|| {
                        parent_selection.map(|selection| {
                            astra_turn_types::ModelSelector::OfferingId {
                                offering_id: selection.offering_id.clone(),
                            }
                        })
                    })
                    .ok_or_else(|| {
                        "child has no admitted parent or default Offering".to_string()
                    })?;
                let reasoning = ReasoningSelection::from(child_thinking.clone());
                let inherited_reasoning = if input.reasoning.is_none()
                    && matches!(
                        selector,
                        astra_turn_types::ModelSelector::ConfiguredName { .. }
                    ) {
                    context
                        .parent_model_reasoning
                        .as_ref()
                        .map(|parent| {
                            Ok::<_, String>(
                                astra_server_types::ModelAdmissionReasoningInheritanceV1 {
                                    offering_id: parent.selection.offering_id.clone(),
                                    reasoning: serde_json::to_value(ReasoningSelection::from(
                                        parent.thinking.clone(),
                                    ))
                                    .map_err(|error| error.to_string())?,
                                },
                            )
                        })
                        .transpose()?
                } else {
                    None
                };
                let slot = ModelAdmissionSlotV1 {
                    selector,
                    max_output_tokens: input.max_output_tokens,
                    reasoning: serde_json::to_value(reasoning)
                        .map_err(|error| error.to_string())?,
                    inherited_reasoning,
                };
                let key = serde_json::to_string(&slot).map_err(|error| error.to_string())?;
                let slot_index = if let Some(slot_index) = distinct_slots.get(&key) {
                    *slot_index
                } else {
                    let slot_index = slots.len();
                    slots.push(slot);
                    distinct_slots.insert(key, slot_index);
                    slot_index
                };
                slot_indexes.push(slot_index);
            }
            let admitted = crate::cli::session::session_runtime::admit_server_model_slots(
                &self.api,
                token.as_deref().expect("batch admission requires token"),
                ModelAdmissionRequestV1 { slots },
            )
            .await?;
            if admitted.len() != distinct_slots.len() {
                return Err("batch model admission returned an incomplete Offering set".into());
            }
            let mut selections = Vec::with_capacity(inputs.len());
            for (index, ((selector, input), slot_index)) in requested_selectors
                .iter()
                .zip(inputs)
                .zip(&slot_indexes)
                .enumerate()
            {
                let admitted = admitted.get(*slot_index).ok_or_else(|| {
                    "batch model admission returned an invalid slot index".to_string()
                })?;
                match selector {
                    Some(astra_turn_types::ModelSelector::OfferingId { offering_id })
                        if admitted.model.offering_id != *offering_id =>
                    {
                        return Err("batch model admission returned a mismatched Offering".into());
                    }
                    Some(astra_turn_types::ModelSelector::ConfiguredName {
                        model_name, ..
                    }) if !admitted.model.name.eq_ignore_ascii_case(model_name) => {
                        return Err(
                            "batch model admission returned a mismatched configured name".into(),
                        );
                    }
                    _ => {}
                }
                if input
                    .resolved_model_selection
                    .as_ref()
                    .is_some_and(|prepared| prepared.offering_id != admitted.model.offering_id)
                {
                    return Err("resolved child Offering changed during batch admission".into());
                }
                thinking[index] = admitted.thinking.clone();
                selections.push(admitted.model.clone());
            }
            selections
        } else if let Some((selection, model_name)) = parent_model_snapshot {
            vec![
                crate::cli::session::session_runtime::ServerModelSelection {
                    pricing: None,
                    name: model_name.to_string(),
                    context_window: None,
                    offering_id: selection.offering_id.clone(),
                };
                inputs.len()
            ]
        } else {
            let model = crate::cli::skill_subrun::resolve_subrun_model_selection(
                &self.api,
                token.as_deref().expect("default resolution requires token"),
                self.default_model.as_deref(),
            )
            .await?;
            vec![model; inputs.len()]
        };
        inputs
            .iter()
            .zip(selections)
            .zip(thinking)
            .map(|((input, model), thinking)| {
                let resolved_selection = Some(astra_turn_types::ModelSelection {
                    offering_id: model.offering_id.clone(),
                });
                Ok(Box::new(CliPreparedSpawn {
                    executor: Arc::clone(&self),
                    parent_run_id: context.parent_run_id.clone(),
                    requested_model_policy: input.requested_model_policy.clone(),
                    resolved_selection,
                    reasoning: thinking,
                    max_output_tokens: input.max_output_tokens,
                    slot: input.fanout_slot_identity()?,
                    model,
                    model_provenance,
                    isolated: input.isolated,
                    selected_root: context.working_dir.clone(),
                    execution_key: Default::default(),
                    control: None,
                }) as Box<dyn PreparedSpawn>)
            })
            .collect()
    }
}

struct CliPreparedSpawn {
    executor: Arc<CliSpawnAgentExecutor>,
    parent_run_id: String,
    requested_model_policy: Option<astra_turn_types::RequestedModelPolicy>,
    resolved_selection: Option<astra_turn_types::ModelSelection>,
    reasoning: astra_turn_core::thinking_config::ThinkingConfig,
    max_output_tokens: Option<u32>,
    slot: Option<AgentFanoutSlotIdentity>,
    model: crate::cli::session::session_runtime::ServerModelSelection,
    model_provenance: &'static str,
    isolated: bool,
    selected_root: PathBuf,
    execution_key: (String, String),
    control: Option<Arc<CliSpawnExecution>>,
}

struct CliSpawnExecution {
    cancellation: tokio_util::sync::CancellationToken,
    origin: std::sync::Mutex<CancellationOrigin>,
    result: tokio::sync::watch::Sender<Option<Result<SpawnRunResult, String>>>,
}

impl CliSpawnExecution {
    async fn wait(&self) -> Result<SpawnRunResult, String> {
        let mut receipt = self.result.subscribe();
        receipt
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "CLI child execution owner disappeared without settlement".to_string())?
            .clone()
            .expect("settlement receipt was checked")
    }
}

struct CancelCliExecutionOnDrop(Arc<CliSpawnExecution>);
impl Drop for CancelCliExecutionOnDrop {
    fn drop(&mut self) {
        self.0.cancellation.cancel();
    }
}

impl Drop for CliPreparedSpawn {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            control.cancellation.cancel();
            control
                .result
                .send_replace(Some(Err("CLI child was not started".into())));
            self.executor
                .remove_execution(&self.execution_key, &control);
        }
    }
}

impl CliSpawnAgentExecutor {
    fn remove_execution(&self, key: &(String, String), control: &Arc<CliSpawnExecution>) {
        let mut executions = astra_core::sync_poison::recover_mutex_lock(&self.executions);
        if executions
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, control))
        {
            executions.remove(key);
        }
    }
}

#[async_trait]
impl PreparedSpawn for CliPreparedSpawn {
    fn model_identity(&self) -> Option<PreparedSpawnModelIdentity> {
        Some(PreparedSpawnModelIdentity {
            offering_id: self.model.offering_id.clone(),
            model_name: self.model.name.clone(),
            provenance: self.model_provenance,
        })
    }

    fn launch(
        mut self: Box<Self>,
        mut config: SpawnRunConfig,
    ) -> Result<astra_runtime::orchestration::SpawnExecution, String> {
        if config.isolated != self.isolated
            || config.working_dir != self.selected_root
            || config
                .parent_address
                .as_ref()
                .map(|address| address.run_id.as_str())
                != Some(self.parent_run_id.as_str())
            || config.fanout_slot != self.slot
            || config.requested_model_policy != self.requested_model_policy
            || config.resolved_model_selection != self.resolved_selection
            || config.thinking != self.reasoning
            || config.max_output_tokens != self.max_output_tokens
        {
            return Err(
                "prepared CLI fanout model does not match its parent, slot, Offering, or reasoning"
                    .to_string(),
            );
        }
        let key = (
            config.run_id.clone(),
            config.cancellation_binding_id.clone(),
        );
        let mut executions = astra_core::sync_poison::recover_mutex_lock(&self.executor.executions);
        if executions.contains_key(&key) {
            return Err("CLI invocation identity already installed".into());
        }
        let (result, _) = tokio::sync::watch::channel(None);
        let control = Arc::new(CliSpawnExecution {
            cancellation: crate::cli::skill_subrun::child_cancellation_scope(
                self.executor.cancel_token.as_ref(),
            )
            .as_ref()
            .clone(),
            origin: std::sync::Mutex::new(CancellationOrigin::Runtime),
            result,
        });
        executions.insert(key.clone(), Arc::clone(&control));
        self.execution_key = key;
        self.control = Some(control);
        drop(executions);
        Ok(Box::pin(async move {
            let control = self
                .control
                .take()
                .ok_or("CLI prepared execution was not installed")?;
            let key = self.execution_key.clone();
            let sink = config.live_event_sink.clone();
            let emitter = config.progress_emitter.clone();
            let run_id = config.run_id.clone();
            let agent_id = config.agent_id.clone();
            let started = std::time::Instant::now();
            let owner_control = Arc::clone(&control);
            tokio::spawn(async move {
                let selected_root = config.working_dir.clone();
                let deadline_timer = config.execution_deadline.map(|deadline| {
                    let token = owner_control.cancellation.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(deadline.monotonic_deadline().into()).await;
                        token.cancel();
                    })
                });
                let mut worktree = None;
                let provisioned = if config.isolated {
                    let root = config.working_dir.clone();
                    let run = run_id.clone();
                    let cancellation = owner_control.cancellation.clone();
                    match tokio::task::spawn_blocking(move || {
                        edge_tools::worktree::provision_child_worktree(&root, &run, &cancellation)
                    })
                    .await
                    {
                        Ok((handle, result)) => {
                            worktree = handle;
                            result
                        }
                        Err(error) => Err(format!(
                            "child workspace provisioning worker failed: {error}"
                        )),
                    }
                } else {
                    Ok(())
                };
                let result = match provisioned {
                    Err(error) => Err(error),
                    Ok(()) if owner_control.cancellation.is_cancelled() => {
                        Err("CLI child cancelled before inference".into())
                    }
                    Ok(()) => {
                        if let Some(handle) = worktree.as_ref() {
                            config.working_dir = handle.path.clone();
                        }
                        use futures_util::FutureExt;
                        std::panic::AssertUnwindSafe(self.executor.execute_with_model_selection(
                            config,
                            self.model.clone(),
                            Arc::new(owner_control.cancellation.clone()),
                        ))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err("CLI child execution panicked".into()))
                    }
                };
                if let Some(timer) = deadline_timer {
                    timer.abort();
                }
                let child_root = worktree.as_ref().map(|handle| handle.path.clone());
                wait_for_cli_workspace_ownership(
                    &selected_root,
                    child_root.as_deref(),
                    sink.as_ref(),
                    &run_id,
                    &agent_id,
                )
                .await;
                // Cleanup is owned independently from observers and uses a fresh
                // bounded token: cancelling the child must not cancel its cleanup.
                while let Some(handle) = worktree.take() {
                    let mut worker_handle = handle.clone();
                    let cancellation = tokio_util::sync::CancellationToken::new();
                    let timer_token = cancellation.clone();
                    let timer = tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        timer_token.cancel();
                    });
                    let cleanup = tokio::task::spawn_blocking(move || {
                        edge_tools::worktree::cleanup_child_worktree(
                            &mut worker_handle,
                            &cancellation,
                        )
                    })
                    .await;
                    timer.abort();
                    match cleanup {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(%run_id, %error, workspace = %handle.path.display(), "CLI child cleanup remains unsettled");
                            emit_agent_execution_waiting(sink.as_ref(), &run_id, &agent_id, error);
                            worktree = Some(handle);
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                        Err(error) => {
                            // An unknown cleanup outcome cannot authorize Terminal.
                            // Keep the control unresolved for exact cancellation retries.
                            tracing::error!(%run_id, %error, "CLI child cleanup worker lost its resource receipt");
                            worktree = Some(handle);
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
                // Cleanup can itself launch Git. Its exit and directory removal
                // cannot replace a confirmed invocation ownership receipt.
                wait_for_cli_workspace_ownership(
                    &selected_root,
                    child_root.as_deref(),
                    sink.as_ref(),
                    &run_id,
                    &agent_id,
                )
                .await;
                let result = if owner_control.cancellation.is_cancelled() {
                    let origin =
                        *astra_core::sync_poison::recover_mutex_lock(&owner_control.origin);
                    let mut result = result.unwrap_or_else(|error| SpawnRunResult {
                        agent_id: agent_id.clone(),
                        run_id: run_id.clone(),
                        committed_frontier: None,
                        status: "cancelled".into(),
                        finish_reason: "cancelled".into(),
                        cancellation_origin: origin,
                        output: None,
                        error: Some(error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                        turns_completed: 0,
                        permission_summary: None,
                        permission_requests: 0,
                        permission_requests_approved: 0,
                        tools_blocked: 0,
                    });
                    result.status = "cancelled".into();
                    result.finish_reason = "cancelled".into();
                    result.cancellation_origin = origin;
                    Ok(result)
                } else {
                    result
                };
                publish_cli_spawn_outcome(
                    &result,
                    sink.as_ref(),
                    emitter.as_ref(),
                    &run_id,
                    &agent_id,
                    started,
                );
                owner_control.result.send_replace(Some(result));
                self.executor.remove_execution(&key, &owner_control);
            });
            let _cancel_on_drop = CancelCliExecutionOnDrop(Arc::clone(&control));
            control.wait().await
        }))
    }
}

async fn wait_for_cli_workspace_ownership(
    selected_root: &std::path::Path,
    child_root: Option<&std::path::Path>,
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
) {
    // A process error or successful exit without scope ownership is not a
    // completion receipt. Preserve the existing workspace fence and control.
    while std::iter::once(selected_root)
        .chain(child_root)
        .any(|root| {
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(root) == Some(true)
        })
    {
        emit_agent_execution_waiting(
            sink,
            run_id,
            agent_id,
            "workspace process ownership remains unsettled".into(),
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Publish execution outcome only after its workspace owner has settled.
fn publish_cli_spawn_outcome(
    result: &Result<SpawnRunResult, String>,
    sink: Option<&SharedAgentLiveEventSink>,
    emitter: Option<&astra_turn_core::orchestration_progress::AgentProgressEmitter>,
    run_id: &str,
    agent_id: &str,
    started: std::time::Instant,
) {
    use astra_turn_core::agent_live_event::AgentLiveTermination;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(emitter) = emitter {
                emitter.failed(error);
            }
            emit_agent_terminated(
                sink,
                run_id,
                agent_id,
                started,
                AgentLiveTermination::Failed,
                Some(error.clone()),
            );
            return;
        }
    };
    let summary = result.output.as_deref().unwrap_or("");
    let preview = summary.chars().take(100).collect::<String>();
    let reason = result
        .error
        .as_deref()
        .or(result.output.as_deref())
        .unwrap_or(&result.finish_reason);
    let duration = started.elapsed().as_millis() as u64;
    let tokens = (result.prompt_tokens, result.completion_tokens);
    let termination = match result.status.as_str() {
        "waiting" | "paused" => {
            if let Some(emitter) = emitter {
                emitter.waiting(reason);
            }
            emit_agent_execution_waiting(sink, run_id, agent_id, reason.into());
            return;
        }
        "cancelled" => {
            let reason = if result.cancellation_origin == CancellationOrigin::User {
                "cancelled by user"
            } else {
                "cancelled by runtime"
            };
            if let Some(emitter) = emitter {
                emitter.cancelled(reason, result.cancellation_origin);
            }
            AgentLiveTermination::Cancelled
        }
        "failed" => {
            if let Some(emitter) = emitter {
                emitter.failed(reason);
            }
            AgentLiveTermination::Failed
        }
        "interrupted" => {
            if let Some(emitter) = emitter {
                emitter.interrupted(
                    &result.finish_reason,
                    preview,
                    result.tool_calls,
                    tokens,
                    duration,
                );
            }
            AgentLiveTermination::Interrupted
        }
        "delegated" => AgentLiveTermination::Delegated,
        _ => {
            if let Some(emitter) = emitter {
                emitter.completed(preview, result.tool_calls, tokens, duration);
            }
            AgentLiveTermination::Completed
        }
    };
    emit_agent_terminated(
        sink,
        run_id,
        agent_id,
        started,
        termination,
        Some(result.finish_reason.clone()),
    );
}

impl CliSpawnAgentExecutor {
    async fn execute_with_model_selection(
        &self,
        config: SpawnRunConfig,
        prepared_model: crate::cli::session::session_runtime::ServerModelSelection,
        child_cancel_token: Arc<tokio_util::sync::CancellationToken>,
    ) -> Result<SpawnRunResult, String> {
        let runtime_ceiling = astra_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(false)?;
        let explicit_hard_limit = config
            .hard_turn_limit
            .map(|turns| {
                std::num::NonZeroUsize::new(turns as usize)
                    .ok_or_else(|| "hard_turn_limit must be positive".to_string())
            })
            .transpose()?;
        let all_schemas = edge_tools::local_tool_schemas();
        let valid_tool_names = tool_names_from_schemas(&all_schemas);

        // Hold a clone for emitting the terminal `AgentTerminated`
        // event after the agentic loop returns. Without this, a
        // crashed / timed-out / cancelled sub-agent would leave its
        // multi_agent strip row stuck in the `live` state forever
        // (reviewer L2-5 — UX C1 + Arch M1).
        let live_event_sink_for_terminal = config.live_event_sink.clone();
        let run_id_for_terminal = config.run_id.clone();
        let agent_id_for_terminal = config.agent_id.clone();

        // A child transcript is inspectable from the moment the run exists,
        // not only after its tool receipt reaches the parent. This start fact
        // is critical (not token telemetry), so the TUI can open the local
        // canonical journal even if fanout reporting later fails.
        if let Some(sink) = config.live_event_sink.as_ref()
            && let Err(error) = sink.send(astra_turn_core::agent_live_event::AgentLiveEvent {
                run_id: config.run_id.clone(),
                agent_id: config.agent_id.clone(),
                kind: astra_turn_core::agent_live_event::AgentLiveEventKind::Signal(
                    astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                        parent_run_id: config
                            .inherited_prefix
                            .as_ref()
                            .map(|prefix| prefix.parent_run_id.clone()),
                        depth: u32::from(config.recursion_depth),
                        spawn_tool_call_id: config.spawn_tool_call_id.clone(),
                        transcript_location:
                            astra_turn_types::AgentTranscriptLocation::LocalJournal,
                    },
                ),
            })
        {
            tracing::debug!(
                agent_id = %config.agent_id,
                run_id = %config.run_id,
                ?error,
                "spawned agent start was not delivered to the live workbench"
            );
        }

        let mut inherited_permissions: InheritedPermissions = config.inherited_permissions.clone();
        inherited_permissions.read_only_execution |= config.read_only;
        let read_only_execution = inherited_permissions.read_only_execution;
        let perm_manager = super::permission_manager::PermissionManager::with_inherited(
            &self.project_root,
            inherited_permissions,
        );
        if read_only_execution {
            let mut context = config.permission_context.write().await;
            context.inherited.read_only_execution = true;
        }

        // Use the working directory from config (may be a worktree)
        let effective_root = config.working_dir.clone();
        // This local sub-run input carries a model alias but no authoritative
        // deployment capability. Use the neutral deterministic strategy; the
        // server applies the admitted provider capability at inference time.
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::default();

        // Resolve the freshest token at spawn time. Without this,
        // sub-agents fail with 401 in long-running sessions after the
        // parent's auth refresh rotates the token (session 82ff91e5).
        let token = match self.resolve_token_async().await {
            Ok(token) => token,
            Err(err) => {
                return Err(err);
            }
        };
        let model_selection = prepared_model;
        let effective_model = Some(model_selection.name);

        let mut executor = edge_tools::ToolExecutor::new(&effective_root)
            .with_cloud(self.api.api_origin(), &token)
            .with_memory_attribution_id(config.run_id.clone())
            .require_delegation_admission(matches!(
                &config.delegated_model_requirements,
                astra_turn_types::DelegationIntentRequirements::Requirements { .. }
                    | astra_turn_types::DelegationIntentRequirements::Unresolved { .. }
                    | astra_turn_types::DelegationIntentRequirements::Unavailable { .. }
            ));
        if read_only_execution {
            executor.set_read_only_execution();
        }
        executor.set_cli_local_provider_schemas(all_schemas.clone());
        if let Some(ref cmds) = self.bg_task_commands {
            executor = executor.with_bg_task_commands(cmds.clone());
        }
        if let Some(ref cache) = self.bg_task_list_cache {
            executor = executor.with_bg_task_list_cache(cache.clone());
        }
        let (active_session_id, journal) = self.session_transcript_snapshot();
        if let Some(session_id) = active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        // Resolve per-model workflow-guard policy once; used for both the
        // `SubRunHost::tool_cache` and the `AgenticLoopState` below.
        let tool_policy_config = astra_config::RuntimeConfig::load().tool_policy;
        let resolved_tool_policy = tool_policy_config.resolve_for_model(effective_model.as_deref());

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: token.clone(),
            model: effective_model.clone(),
            offering_id: model_selection.offering_id,
            requested_model_policy: config.requested_model_policy.clone(),
            project_root: effective_root.clone(),
            executor: std::sync::Arc::new(executor),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            initial_output_limit: config.max_output_tokens,
            effort: None,
            agent_type: Some(config.agent_type.clone()),
            execution_deadline: config.execution_deadline,
            cancel_token: Some(child_cancel_token.clone()),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: None,
            agent_id: config.agent_id.clone(),
            stream_event_tx: None,
            stream_event_sink: agent_live_stream_event_sink(
                config.run_id.clone(),
                config.agent_id.clone(),
                config.live_event_sink.clone(),
            ),
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
            inherited_prefix: config.inherited_prefix.clone(),
            fork_cache_sink: self.fork_cache_sink.clone(),
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal,
            journal_identity: None,
        };

        // Build system message from agent type definition
        // The runtime carries the child identity and parent route in typed
        // context.  Keep random IDs out of the system prefix so sibling
        // children with the same persona can reuse the provider cache.
        let system_prompt = if read_only_execution {
            build_child_system_prompt(&format!(
                "{}\nShell is unavailable on this Runner. Use read_file, grep, and glob; if the task requires Git diff evidence, report that limitation instead of retrying Bash.",
                config.system_prompt_addendum
            ))
        } else {
            build_child_system_prompt(&config.system_prompt_addendum)
        };

        // PR 5.6: if the spawner resolved a parent prefix, prepend
        // the captured prefix messages between the system prompt
        // and the child's own task. System prompt stays at [0] so
        // it reads as the child's own identity; inherited messages
        // sit behind it as "historical context from parent" that
        // the provider can hit in its prompt cache. The child task
        // at the tail is always fresh (cacheable only for future
        // child turns, not for this first call).
        let force_reasoning_field = config.inherited_prefix.as_ref().is_some_and(|ip| {
            ip.thinking
                .as_ref()
                .is_some_and(|thinking| thinking.enabled)
                || astra_turn_core::edge_ledger::history_has_reasoning(&ip.prefix_messages)
        }) || effective_model.as_deref().is_some_and(|model| {
            astra_turn_core::reasoning_capabilities::reasoning_capabilities("", model)
                .requires_replay()
        });

        let messages = build_child_messages(
            &system_prompt,
            config
                .inherited_prefix
                .as_ref()
                .map(|ip| ip.prefix_messages.as_slice()),
            &config.task,
            force_reasoning_field,
            &config.run_id,
        );
        if host.journal.is_some() {
            if let Some(session_id) = active_session_id.as_ref() {
                host.journal_identity = Some(SubRunJournalIdentity {
                    session_id: session_id.clone(),
                    run_id: config.run_id.clone(),
                    parent_run_id: config
                        .parent_address
                        .as_ref()
                        .map(|address| address.run_id.clone()),
                    next_item_seq: 1,
                    last_assistant_source_event_id: None,
                    persistence_blocked: false,
                });
            } else {
                tracing::warn!(
                    agent_id = %config.agent_id,
                    run_id = %config.run_id,
                    "child journal is configured without a parent session identity"
                );
            }
        }

        // Build restricted tools based on agent type's allowed_tools
        let restricted_tools = if config.allowed_tools.iter().any(|t| t == "*") {
            // All tools allowed
            HashSet::new()
        } else {
            // Only allow specified tools.
            build_restricted_tools(Some(config.allowed_tools.as_slice()), &valid_tool_names)
        };

        // Add edit/create to restricted if read_only
        let restricted_tools = if read_only_execution {
            let mut restricted = restricted_tools;
            restricted.insert("edit".to_string());
            restricted.insert("create".to_string());
            restricted.insert("write_file".to_string());
            restricted.insert("str_replace".to_string());
            restricted.insert("bash".to_string());
            restricted.insert("lsp".to_string());
            restricted
        } else {
            restricted_tools
        };

        let task_profile = if config.read_only {
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                false,
                true,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            )
        } else {
            infer_task_execution_profile(&config.task)
        };
        // Local step-recorder session id: kept synthetic (`spawn-...`)
        // because it's only used for local journal / step file
        // persistence — server never sees this.
        let local_subrun_session_id = format!("spawn-{}-{}", config.run_id, config.agent_id);
        let user_id = cli_user_id();
        let step_recorder = StepRecorder::with_persistence_for_run(
            &user_id,
            &local_subrun_session_id,
            &format!("{}-run", config.run_id),
            &config.run_id,
        );

        // Wire session for the *server-facing* `chat_turn_base_payload`:
        // pass None so the server opens a fresh session for this child
        // turn rather than rejecting a synthetic `spawn-...` id with
        // "Session not found" — discovered during real-world MiniMax
        // spawn_agent verification. Reusing the parent's active
        // session id would risk cross-contamination when multiple
        // children share one parent, and cross-child race conditions
        // on per-session state.
        //
        // Local continuity (transcript, step recorder, tool journal)
        // still uses `local_subrun_session_id` which is client-side
        // only, so children remain traceable offline.
        let server_session_id: Option<String> = None;

        let has_parent_permissions = config.parent_address.is_some();

        let agentic_turn_budget =
            astra_turn_core::chat_turn_heuristics::resolve_spawned_agentic_turn_budget(
                task_profile,
                runtime_ceiling,
                config.initial_turns as usize,
                explicit_hard_limit,
            );

        let child_thinking = config.thinking.clone();
        let child_model_requirements = config.delegated_model_requirements.clone();
        let runtime_manifest = runtime_manifest_for_model(
            "cli_spawn_subrun",
            "cli_spawn_subrun",
            effective_model.as_deref(),
        );

        let mut state = AgenticLoopState {
            messages,
            current_session_id: server_session_id,
            current_run_id: Some(config.run_id.clone()),
            context_manifest_user_id: Some(user_id),
            context_manifest_model_name: effective_model,
            runtime_manifest,
            recursion_depth: config.recursion_depth,
            budget_is_explicit: config.hard_turn_limit.is_some(),
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            skills: build_child_skill_state(
                astra_runtime::turn::agentic_loop::host::RequestConstraints {
                    delegated_model_requirements: child_model_requirements,
                    ..Default::default()
                },
                self.skill_resolver.clone(),
                &effective_root,
            ),
            hooks: StopHookState {
                workspace_root_hint: Some(effective_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                progress_emitter: config.progress_emitter,
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: Some(child_cancel_token),
                execution_lease_lost: None,
                resolved_origin: None,
            },
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    astra_runtime::turn::session_current_date::resolve_session_current_date(
                        active_session_id.as_deref().unwrap_or(""),
                    ),
                ),
            ),
            message: config.task.clone(),
            user_intent: config.task.clone(),
            task_profile,
            api_token: token.clone(),
            self_agent_id: "spawn_subrun".to_string(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            thinking: child_thinking,
            permission_context: Some(config.permission_context),
            compact_strategy,
            canonical_turn_chain_id: Some(config.run_id.clone()),
            root_user_query_event_id: Some(format!("{}:initial-user-query", config.run_id)),
            ..AgenticLoopState::fresh(
                step_recorder,
                agentic_turn_budget,
                &resolved_tool_policy,
                astra_turn_types::InferencePurpose::SubAgent,
                astra_runtime::turn::runtime_policy::evaluation_thresholds_from_policy(
                    &tool_policy_config,
                ),
                self.api.clone(),
            )
        };

        // Inherit skills from parent: pre-populate discovered skills
        if !config.inherited_skills.is_empty() {
            for skill_name in &config.inherited_skills {
                state.skills.execution.discovered.insert(skill_name.clone());
            }
        }

        // The child's own task is canonical user conversation, not merely a
        // launch-card description. Persist it before the first network/model
        // boundary so opening a newly started local agent has the same useful
        // transcript seed as a durable server run. System and inherited prefix
        // are provider context, so only the final child task starts the
        // canonical run transcript.
        if host.journal_identity.is_some() {
            state.begin_run_transcript_capture(state.messages.last().cloned());
        }
        let _ = host.flush_agent_transcript(&state);

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Preserve the loop's explicit lifecycle. Tool failures remain in the
        // transcript/evaluation lane; they do not manufacture an interruption
        // after a child run has completed.
        // The happy-path hook normally keeps this current. A terminal flush
        // retains tool messages appended after the last successful ingest and
        // partial history from cancelled/failed loops.
        if let Some(source_event_id) = host.finalize_agent_transcript(&state).await {
            emit_agent_transcript_committed(
                live_event_sink_for_terminal.as_ref(),
                &run_id_for_terminal,
                &agent_id_for_terminal,
                source_event_id,
                astra_turn_types::AgentTranscriptLocation::LocalJournal,
            );
        }

        let tool_calls = state.total_tool_calls as u32;
        let turns_completed = state.current_session_turn_number();
        let agent_id = config.agent_id.clone();
        let run_id = config.run_id;
        let prompt_tokens = state.total_prompt;
        let completion_tokens = state.total_completion;
        let ctx = match state.permission_context.as_ref() {
            Some(ctx) => ctx,
            None => {
                // Fail with an actionable error rather than panicking the
                // whole process. A missing permission_context means the
                // spawn path didn't wire runtime permissions into the child
                // agent state — surface it so the caller can fix the config
                // instead of crashing the CLI.
                return Err(format!(
                    "spawned agent {agent_id} (run_id={run_id}) completed without a runtime permission_context; \
                     the spawn path must install `permission_context` into SpawnRunConfig before delegating to the agentic loop"
                ));
            }
        };
        let ctx_guard = ctx.read().await;
        let telemetry = ctx_guard.telemetry();
        let mode = match ctx_guard.mode() {
            astra_runtime::orchestration::PermissionMode::Auto => "auto".to_string(),
            astra_runtime::orchestration::PermissionMode::Bypass => "bypass".to_string(),
            astra_runtime::orchestration::PermissionMode::Plan => "plan".to_string(),
            astra_runtime::orchestration::PermissionMode::AcceptEdits => "accept_edits".to_string(),
            astra_runtime::orchestration::PermissionMode::Prompt => "prompt".to_string(),
            astra_runtime::orchestration::PermissionMode::Deny => "deny".to_string(),
        };
        let permission_summary = Some(PermissionSummary {
            mode,
            allow_rules: ctx_guard.effective_allow_rule_count(),
            deny_rules: ctx_guard.effective_deny_rule_count(),
            has_parent: has_parent_permissions,
            recent_denials: telemetry.recent_denials.clone(),
        });
        let permission_requests = telemetry.permission_requests;
        let permission_requests_approved = telemetry.permission_requests_approved;
        let tools_blocked = telemetry.tools_blocked;
        drop(ctx_guard);

        // Derive finish_reason from the structured interruption
        // record if present. This surfaces budget exhaustion /
        // token budget exceeded / context overflow as first-class
        // signals even when the legacy `status` remains
        // `"completed"` — which is how the loop reports all
        // resumable interruptions. Parents (and agent get_result)
        // can now switch on this field without regex-matching
        // the output.
        let finish_reason_from_state = state
            .interruption
            .as_ref()
            .map(|i| i.kind.label().to_string());
        // Helper: tell any live event sink that the sub-agent has
        // reached a terminal state. The TUI's `agent_runs` registry
        // is updated via this signal so the strip row flips from ◦
        // (live) to ✓ / ✗ / ⊘ (terminal). Without this, a crashed or
        // timed-out child leaves the row stuck in `live`.

        match loop_result {
            Ok(AgenticLoopOutcome::Delegated) => Ok(SpawnRunResult {
                agent_id,
                run_id,
                committed_frontier: None,
                status: "delegated".to_string(),
                finish_reason: "delegated".to_string(),
                cancellation_origin: CancellationOrigin::Unverified,
                output: None,
                error: None,
                prompt_tokens,
                completion_tokens,
                tool_calls,
                turns_completed,
                permission_summary,
                permission_requests,
                permission_requests_approved,
                tools_blocked,
            }),
            Ok(AgenticLoopOutcome::ControlRejected(rejection)) => {
                let error = format!("{}: {}", rejection.code, rejection.message);

                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    committed_frontier: None,
                    status: "failed".to_string(),
                    finish_reason: "terminal_control_rejected".to_string(),
                    cancellation_origin: CancellationOrigin::Unverified,
                    output: None,
                    error: Some(error),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    turns_completed,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Completed) => Ok(SpawnRunResult {
                agent_id,
                run_id,
                committed_frontier: None,
                status: spawn_completion_status_from_finish_reason(
                    finish_reason_from_state.as_deref(),
                )
                .to_string(),
                finish_reason: finish_reason_from_state.unwrap_or_else(|| "normal".to_string()),
                cancellation_origin: CancellationOrigin::Unverified,
                output: Some(state.final_text),
                error: None,
                prompt_tokens,
                completion_tokens,
                tool_calls,
                turns_completed,
                permission_summary,
                permission_requests,
                permission_requests_approved,
                tools_blocked,
            }),
            Ok(AgenticLoopOutcome::Cancelled) => {
                let cancellation_origin =
                    cancelled_loop_origin(state.interruption.as_ref().map(|record| record.kind));
                let projection = project_subrun_status_to_spawn(astra_core::STATUS_CANCELLED, None);
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    committed_frontier: None,
                    status: projection.status.to_string(),
                    finish_reason: finish_reason_from_state
                        .unwrap_or_else(|| projection.finish_reason.to_string()),
                    cancellation_origin,
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    turns_completed,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Error(error)) => {
                // Emit failed event

                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    committed_frontier: None,
                    status: "failed".to_string(),
                    finish_reason: finish_reason_from_state.unwrap_or_else(|| "failed".to_string()),
                    cancellation_origin: CancellationOrigin::Unverified,
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: Some(error),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    turns_completed,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                let projection = project_subrun_status_to_spawn(astra_core::STATUS_WAITING, None);
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    committed_frontier: None,
                    status: projection.status.to_string(),
                    finish_reason: finish_reason_from_state
                        .unwrap_or_else(|| projection.finish_reason.to_string()),
                    cancellation_origin: CancellationOrigin::Unverified,
                    output: Some(reason),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    turns_completed,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Err(e)
                if let Some(cancellation_origin) = classified_error_cancellation_origin(e.kind) =>
            {
                let projection = project_subrun_status_to_spawn(astra_core::STATUS_CANCELLED, None);
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    committed_frontier: None,
                    status: projection.status.to_string(),
                    finish_reason: finish_reason_from_state
                        .unwrap_or_else(|| projection.finish_reason.to_string()),
                    cancellation_origin,
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    turns_completed,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Err(e) => {
                let msg = e.to_string();

                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CliSpawnAgentExecutor, TokenProvider, agent_live_stream_event_sink, build_child_messages,
        build_child_system_prompt, cancelled_loop_origin, classified_error_cancellation_origin,
        emit_agent_transcript_committed,
    };
    use crate::lock_recovery::LockRecovery;
    use astra_runtime::orchestration::{
        InheritedPermissions, PermissionMode, PermissionSyncContext, SpawnAgentExecutor,
        SpawnAgentInput, SpawnContext, SpawnRunConfig,
    };
    use astra_services::{ModelAccessKind, ModelExecutionPlacement, ModelListItemResponse};
    use astra_turn_core::agent_live_event::{
        AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveSendError,
        SharedAgentLiveEventSink,
    };
    use astra_turn_core::interruption::InterruptionKind;
    use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;
    use astra_turn_core::orchestration_types::CancellationOrigin;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cli::chat_stream::StreamEvent;

    fn cli_fanout_test_context() -> SpawnContext {
        SpawnContext {
            parent_profile_authority: astra_runtime::orchestration::ParentProfileAuthority::Unbound,
            admitted_agent_profiles: None,
            parent_run_id: "parent-run".into(),
            parent_agent_id: "parent-agent".into(),
            resolved_model_name: Some("parent-model".into()),
            delegation_model_admission: None,
            parent_model_reasoning: None,
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: InheritedPermissions::auto_approve(),
            inherited_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            workspace_mutation: Default::default(),
            delegation_chain: Vec::new(),
        }
    }

    fn prepared_cli_test_config(
        slot: Option<AgentFanoutSlotIdentity>,
        model_selection: Option<astra_turn_types::ModelSelection>,
        thinking: astra_turn_core::thinking_config::ThinkingConfig,
    ) -> SpawnRunConfig {
        let (inherited_permissions, permission_context) = test_permission_context();
        SpawnRunConfig {
            run_id: "child-run".into(),
            cancellation_binding_id: "child-binding".into(),
            agent_id: "child@run".into(),
            spawn_tool_call_id: None,
            recursion_depth: 1,
            agent_type: "explore".into(),
            description: "test slot".into(),
            task: "reply".into(),
            system_prompt_addendum: String::new(),
            requested_model_policy: None,
            resolved_model_selection: model_selection,
            delegated_model_requirements: Default::default(),
            fanout_slot: slot,
            thinking,
            max_output_tokens: None,
            model: Some("parent-model".into()),
            initial_turns: 1,
            hard_turn_limit: Some(0),
            execution_deadline: None,
            allowed_tools: Vec::new(),
            read_only: true,
            workspace_mutation: Default::default(),
            isolated: false,
            working_dir: PathBuf::from("/tmp"),
            mailbox: None,
            progress_emitter: None,
            inherited_permissions,
            parent_address: Some(astra_messaging::types::AgentAddress::new(
                "parent-run",
                "parent-agent",
            )),
            permission_context,
            inherited_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            inherited_prefix: None,
            execution_metadata: None,
            is_fork_child: false,
            delegation_chain: Vec::new(),
            profile_authority: astra_runtime::orchestration::ParentProfileAuthority::Unbound,
            admitted_agent_profiles: None,
            work_item: None,
        }
    }

    async fn execute_prepared_cli_for_test(
        preparation: Box<dyn astra_runtime::orchestration::PreparedSpawn>,
        config: SpawnRunConfig,
    ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
        preparation.launch(config)?.await
    }

    async fn execute_cli_for_test(
        executor: Arc<CliSpawnAgentExecutor>,
        mut config: SpawnRunConfig,
    ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
        let mut context = cli_fanout_test_context();
        context.working_dir = config.working_dir.clone();
        context.resolved_model_name = config.model.clone();
        if let Some(address) = config.parent_address.as_ref() {
            context.parent_run_id = address.run_id.clone();
            context.parent_agent_id = address.agent_id.clone();
        } else {
            config.parent_address = Some(astra_messaging::AgentAddress::new(
                &context.parent_run_id,
                &context.parent_agent_id,
            ));
        }
        let parent = config.resolved_model_selection.clone();
        context.parent_model_reasoning = parent.clone().map(|selection| {
            astra_turn_core::orchestration_spawn_tool::ParentModelReasoning {
                selection,
                resolved_model_name: config.model.clone(),
                thinking: config.thinking.clone(),
            }
        });
        let input = SpawnAgentInput {
            requested_model_policy: config.requested_model_policy.clone(),
            resolved_model_selection: config.resolved_model_selection.clone(),
            reasoning: parent.is_none().then(|| config.thinking.clone().into()),
            max_output_tokens: config.max_output_tokens,
            isolated: config.isolated,
            ..Default::default()
        };
        let mut prepared = executor
            .prepare_batch(&[input], &context, parent.as_ref())
            .await?;
        execute_prepared_cli_for_test(prepared.remove(0), config).await
    }

    fn prepared_cli_test_config_for_input(
        input: &SpawnAgentInput,
        model_selection: Option<astra_turn_types::ModelSelection>,
        thinking: astra_turn_core::thinking_config::ThinkingConfig,
    ) -> SpawnRunConfig {
        let mut config = prepared_cli_test_config(
            input.fanout_slot_identity().expect("valid test slot"),
            model_selection,
            thinking,
        );
        config.requested_model_policy = input.requested_model_policy.clone();
        config
    }

    fn test_executor(base_url: &str) -> CliSpawnAgentExecutor {
        CliSpawnAgentExecutor::new(
            astra_thin_client::ThinClient::new(base_url, None).expect("test api"),
            "token".into(),
            PathBuf::from("/tmp"),
            None,
        )
    }

    fn test_offering(offering_id: &str, name: &str) -> ModelListItemResponse {
        ModelListItemResponse {
            thinking_protocol: None,
            offering_id: offering_id.into(),
            access_id: "self-hosted".into(),
            access_kind: ModelAccessKind::SelfHosted,
            access_label: "Self-hosted".into(),
            execution_placement: ModelExecutionPlacement::Server,
            name: name.into(),
            provider: "openai".into(),
            description: None,
            is_active: true,
            context_window: 128_000,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
            pricing: None,
        }
    }

    #[tokio::test]
    async fn unpolled_cli_launch_stays_unsettled_until_its_future_is_dropped() {
        let server = MockServer::start().await;
        let executor = Arc::new(test_executor(&server.uri()));
        let repo = crate::edge_tools::tests::worktree_tests::init_temp_git_repo();
        let mut context = cli_fanout_test_context();
        context.working_dir = repo.path().to_path_buf();
        let input = SpawnAgentInput {
            isolated: true,
            agent_type: "task".into(),
            ..Default::default()
        };
        let parent = astra_turn_types::ModelSelection {
            offering_id: "offer-parent".into(),
        };
        let preparation = Arc::clone(&executor)
            .prepare_batch(&[input], &context, Some(&parent))
            .await
            .unwrap()
            .remove(0);
        let mut config = prepared_cli_test_config(
            None,
            Some(parent),
            astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
        );
        config.run_id = uuid::Uuid::new_v4().to_string();
        config.working_dir = repo.path().to_path_buf();
        config.isolated = true;
        let run_id = config.run_id.clone();
        let binding = config.cancellation_binding_id.clone();
        let execution = preparation.launch(config).unwrap();
        assert_eq!(executor.executions.lock_recover().len(), 1);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                executor.cancel_spawned_run_durably(
                    &run_id,
                    Some(&binding),
                    None,
                    "stop before poll",
                    super::CancellationOrigin::User
                )
            )
            .await
            .is_err(),
            "an outstanding registered future is not a stopped execution"
        );
        assert_eq!(executor.executions.lock_recover().len(), 1);
        assert!(!repo.path().join(".agent-worktrees").exists());
        drop(execution);
        assert!(executor.executions.lock_recover().is_empty());
        executor
            .cancel_spawned_run_durably(
                &run_id,
                Some(&binding),
                None,
                "repeat stop",
                super::CancellationOrigin::User,
            )
            .await
            .unwrap();
        assert!(!repo.path().join(".agent-worktrees").exists());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsettled_workspace_ownership_never_produces_a_cli_terminal_receipt() {
        let server = MockServer::start().await;
        let executor = Arc::new(test_executor(&server.uri()));
        let root = tempfile::tempdir().unwrap();
        astra_tools::workspace_observation::WorkspaceAttributionState::capture(root.path())
            .unwrap()
            .mark_unsettled();
        let mut config = prepared_cli_test_config(
            None,
            Some(astra_turn_types::ModelSelection {
                offering_id: "offer-parent".into(),
            }),
            astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
        );
        config.run_id = uuid::Uuid::new_v4().to_string();
        config.isolated = true;
        config.working_dir = root.path().to_path_buf();
        let run_id = config.run_id.clone();
        let binding = config.cancellation_binding_id.clone();
        let task = tokio::spawn(execute_cli_for_test(executor.clone(), config));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while executor.executions.lock_recover().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                executor.cancel_spawned_run_durably(
                    &run_id,
                    Some(&binding),
                    None,
                    "stop unknown workspace process",
                    super::CancellationOrigin::User
                )
            )
            .await
            .is_err()
        );
        assert!(!task.is_finished());
        assert_eq!(executor.executions.lock_recover().len(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
        task.abort();
        let _ = task.await;
        assert_eq!(
            executor.executions.lock_recover().len(),
            1,
            "dropping the observer cannot manufacture a stop receipt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_workspace_deadline_stops_git_before_inference_and_waits_for_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let server = MockServer::start().await;
        let executor = Arc::new(test_executor(&server.uri()));
        let repo = crate::edge_tools::tests::worktree_tests::init_temp_git_repo();
        let ready = repo.path().join("deadline-hook-ready");
        let late = repo.path().join("deadline-hook-late");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf ready > '{}'\nsleep 7\nprintf late > '{}'\n",
                ready.display(),
                late.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = prepared_cli_test_config(
            None,
            Some(astra_turn_types::ModelSelection {
                offering_id: "offer-parent".into(),
            }),
            astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
        );
        config.run_id = uuid::Uuid::new_v4().to_string();
        config.isolated = true;
        config.working_dir = repo.path().to_path_buf();
        // Admission and Git startup consume the same real absolute budget.
        // Leave startup headroom under suite load while the hook still lasts
        // beyond the budget, then observe past its natural completion time.
        config.execution_deadline = Some(
            astra_services::runs::ExecutionDeadlineAuthority::from_budget_at(
                astra_services::runs::ExecutionTimeBudget {
                    remaining_seconds: 5,
                },
                0,
            )
            .unwrap(),
        );
        let worktree = repo.path().join(".agent-worktrees").join(&config.run_id);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            execute_cli_for_test(executor.clone(), config),
        )
        .await
        .expect("deadline settles real workers")
        .expect("cancelled result");
        assert!(ready.exists(), "Git actually reached the blocking hook");
        assert_eq!(result.status, "cancelled");
        assert_eq!(
            result.cancellation_origin,
            super::CancellationOrigin::Runtime
        );
        assert!(!worktree.exists());
        assert!(executor.executions.lock_recover().is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(7_100)).await;
        assert!(!late.exists(), "deadline cannot leave a delayed writer");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_cli_isolation_waits_for_git_and_cleans_before_terminal() {
        for (unsettled_child, during_checkout) in [(false, false), (false, true), (true, false)] {
            use astra_runtime::orchestration::{
                AgentStatus, DynamicAgentSpawner, SpawnAgentOutput,
            };
            use std::os::unix::fs::PermissionsExt;
            let server = MockServer::start().await;
            let executor = Arc::new(test_executor(&server.uri()));
            let repo = crate::edge_tools::tests::worktree_tests::init_temp_git_repo();
            let ready = repo.path().join("hook-ready");
            let late = repo.path().join("late-hook-write");
            let hook = if during_checkout {
                let filter = repo.path().join("blocking-smudge");
                std::fs::write(
                    repo.path().join(".gitattributes"),
                    "tracked.txt filter=blocking\n",
                )
                .unwrap();
                for args in [
                    vec!["add", ".gitattributes"],
                    vec!["commit", "-m", "configure checkout filter"],
                    vec!["config", "filter.blocking.smudge", filter.to_str().unwrap()],
                    vec!["config", "filter.blocking.required", "true"],
                ] {
                    assert!(
                        std::process::Command::new("git")
                            .args(args)
                            .current_dir(repo.path())
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                filter
            } else {
                repo.path().join(".git/hooks/post-checkout")
            };
            std::fs::write(
                &hook,
                format!(
                    "#!/bin/sh\nprintf ready > '{}'\nsleep {}\nprintf late > '{}'\n",
                    ready.display(),
                    if during_checkout { "3" } else { "0.6" },
                    late.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
                Arc::new(astra_messaging::InProcessTransport::new()),
                Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new()),
            ));
            let spawner = DynamicAgentSpawner::new(router).with_executor(executor.clone());
            let mut context = cli_fanout_test_context();
            context.working_dir = repo.path().to_path_buf();
            context.parent_model_reasoning = Some(
                astra_turn_core::orchestration_spawn_tool::ParentModelReasoning {
                    selection: astra_turn_types::ModelSelection {
                        offering_id: "offer-parent".into(),
                    },
                    resolved_model_name: context.resolved_model_name.clone(),
                    thinking: astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
                },
            );
            let _parent = spawner.fanout_parent(&context.parent_run_id);
            let SpawnAgentOutput::Launched {
                agent_id, run_id, ..
            } = spawner
                .spawn(
                    SpawnAgentInput {
                        isolated: true,
                        agent_type: "task".into(),
                        prompt: "must not reach inference".into(),
                        ..Default::default()
                    },
                    &context,
                )
                .await
                .expect("launch receipt precedes Git provisioning");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !ready.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("real Git hook started");
            if during_checkout {
                let registration = std::process::Command::new("git")
                    .args(["worktree", "list", "--porcelain"])
                    .current_dir(repo.path())
                    .output()
                    .unwrap();
                assert!(
                    String::from_utf8_lossy(&registration.stdout).contains("locked initializing"),
                    "cancel must hit checkout's initialization lock"
                );
            }
            assert!(
                !spawner
                    .get_agent_state_any(&agent_id)
                    .await
                    .unwrap()
                    .status
                    .is_terminal()
            );
            if unsettled_child {
                let child_root = repo.path().join(".agent-worktrees").join(&run_id);
                astra_tools::workspace_observation::WorkspaceAttributionState::capture(&child_root)
                    .unwrap()
                    .mark_unsettled();
                assert_ne!(
                    astra_tools::workspace_observation::workspace_ownership_is_unsettled(
                        repo.path()
                    ),
                    Some(true)
                );
                let binding = spawner
                    .get_agent_state_any(&agent_id)
                    .await
                    .unwrap()
                    .cancellation_binding_id
                    .unwrap();
                assert!(
                    spawner
                        .cancel_agent_for_user(
                            &agent_id,
                            "stop child with unconfirmed process ownership",
                        )
                        .await
                        .is_pending(),
                    "stop request remains pending until the exact executor receipt"
                );
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(80),
                        executor.cancel_spawned_run_durably(
                            &run_id,
                            Some(&binding),
                            None,
                            "observe exact stop receipt",
                            super::CancellationOrigin::User
                        ),
                    )
                    .await
                    .is_err(),
                    "accepted stop is not a settled executor receipt"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert!(
                    child_root.exists(),
                    "unknown child processes forbid deleting their workspace"
                );
                assert_eq!(executor.executions.lock_recover().len(), 1);
                assert!(
                    !spawner
                        .get_agent_state_any(&agent_id)
                        .await
                        .unwrap()
                        .status
                        .is_terminal()
                );
                assert!(server.received_requests().await.unwrap().is_empty());
                continue;
            }
            let _ = spawner
                .cancel_agent_for_user(&agent_id, "stop Git provisioning")
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if spawner
                        .get_agent_state_any(&agent_id)
                        .await
                        .is_some_and(|state| state.status.is_terminal())
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancel waits for actual workers and cleanup");
            let state = spawner.get_agent_state_any(&agent_id).await.unwrap();
            assert!(
                matches!(state.status, AgentStatus::Cancelled { by_user: true, .. }),
                "{:?}",
                state.status
            );
            assert!(!repo.path().join(".agent-worktrees").join(run_id).exists());
            assert!(executor.executions.lock_recover().is_empty());
            let registration = std::process::Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(registration.status.success());
            assert!(!String::from_utf8_lossy(&registration.stdout).contains(".agent-worktrees"));

            assert!(
                server.received_requests().await.unwrap().is_empty(),
                "cancelled provisioning must not start a model round"
            );
            tokio::time::sleep(std::time::Duration::from_millis(if during_checkout {
                3_100
            } else {
                650
            }))
            .await;
            assert!(
                !late.exists(),
                "Git must not outlive its cancellation receipt"
            );
            spawner
                .shutdown_and_wait(std::time::Duration::from_secs(2))
                .await;
        }
    }

    #[tokio::test]
    async fn cli_fanout_reuses_inherited_offering_without_model_access_io() {
        let server = MockServer::start().await;
        let executor = Arc::new(test_executor(&server.uri()));
        let context = cli_fanout_test_context();
        let inputs = (0..2)
            .map(|slot| SpawnAgentInput {
                description: format!("slot {slot}"),
                prompt: "reply".into(),
                fanout_group_id: Some("group".into()),
                fanout_target_count: Some(2),
                fanout_slot_index: Some(slot),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let parent = astra_turn_types::ModelSelection {
            offering_id: "offer-parent".into(),
        };
        let prepared = Arc::clone(&executor)
            .prepare_batch(&inputs, &context, Some(&parent))
            .await
            .expect("reuse the parent's exact Offering and resolved model name");
        assert_eq!(prepared.len(), 2);
        server.verify().await;
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "inherited admission uses no HTTP/DB lookup"
        );
        assert!(prepared.iter().all(|spawn| {
            spawn.model_identity().is_some_and(|identity| {
                identity.offering_id == "offer-parent"
                    && identity.model_name == "parent-model"
                    && identity.provenance == "inherited_parent_context"
            })
        }));

        let wrong_slot = prepared_cli_test_config(
            inputs[0].fanout_slot_identity().unwrap(),
            Some(parent.clone()),
            astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
        );
        let mut prepared = prepared.into_iter();
        let inherited = execute_prepared_cli_for_test(
            prepared.next().unwrap(),
            prepared_cli_test_config(
                inputs[0].fanout_slot_identity().unwrap(),
                Some(parent.clone()),
                astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
            ),
        )
        .await
        .expect_err("valid binding reaches the non-network turn-limit guard");
        assert!(
            inherited.contains("hard_turn_limit must be positive"),
            "{inherited}"
        );
        let error = execute_prepared_cli_for_test(prepared.next().unwrap(), wrong_slot)
            .await
            .expect_err("prepared slot must not execute another slot");
        assert!(error.contains("does not match"), "{error}");
        server.verify().await;

        let mut unsupported = inputs;
        unsupported[1].reasoning =
            Some(astra_turn_core::orchestration_spawn_tool::ReasoningSelection::Off);
        let unsupported_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(400).set_body_string("reasoning unsupported"))
            .expect(1)
            .mount(&unsupported_server)
            .await;
        let unsupported_executor = Arc::new(test_executor(&unsupported_server.uri()));
        let error = match Arc::clone(&unsupported_executor)
            .prepare_batch(&unsupported, &context, Some(&parent))
            .await
        {
            Ok(_) => panic!("unsupported reasoning must reject the entire batch"),
            Err(error) => error,
        };
        assert!(error.contains("reasoning unsupported"), "{error}");
        unsupported_server.verify().await;

        let unavailable_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&unavailable_server)
            .await;
        let unavailable_executor = Arc::new(test_executor(&unavailable_server.uri()));
        unsupported[1].reasoning = None;
        unsupported[1].max_output_tokens = Some(64);
        assert!(
            Arc::clone(&unavailable_executor)
                .prepare_batch(&unsupported, &context, Some(&parent))
                .await
                .is_err(),
            "failed batch admission must reject the group before child creation"
        );
        unavailable_server.verify().await;

        let revoked_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Offering is not active"))
            .expect(1)
            .mount(&revoked_server)
            .await;
        let revoked_executor = Arc::new(test_executor(&revoked_server.uri()));
        unsupported[1].reasoning = None;
        unsupported[1].max_output_tokens = None;
        unsupported[1].requested_model_policy =
            Some(astra_turn_types::RequestedModelPolicy::Fixed {
                selector: astra_turn_types::ModelSelector::OfferingId {
                    offering_id: "offer-revoked".into(),
                },
            });
        let revoked_error = match Arc::clone(&revoked_executor)
            .prepare_batch(&unsupported, &context, Some(&parent))
            .await
        {
            Ok(_) => panic!("revoked explicit Offering must reject the entire batch"),
            Err(error) => error,
        };
        assert!(
            revoked_error.contains("Offering is not active"),
            "{revoked_error}"
        );
        revoked_server.verify().await;
    }

    #[tokio::test]
    async fn cli_spawn_handler_reuses_inherited_offering_without_model_access_admit() {
        let mock = crate::cli::mock_llm::MockLlmServer::start(
            crate::cli::mock_llm::MockScenario::Complete,
        )
        .await
        .expect("mock child SSE server");
        let executor = Arc::new(test_executor(&mock.base_url));
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let spawner = Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(router).with_executor(executor),
        );
        let working_dir = std::env::current_dir().expect("test runs from the repository");
        let context = astra_runtime::orchestration::AgentToolContext {
            parent_profile_authority: astra_runtime::orchestration::ParentProfileAuthority::Unbound,
            admitted_agent_profiles: None,
            delegation_model_admission: None,
            run_id: "parent-run".into(),
            agent_id: "parent-agent".into(),
            delegation_chain: Vec::new(),
            current_model: Some("parent-model".into()),
            current_model_selection: Some(astra_turn_types::ModelSelection {
                offering_id: "offer-parent".into(),
            }),
            parent_model_reasoning: None,
            recursion_depth: 0,
            is_fork_child: false,
            working_dir,
            spawner: spawner.clone(),
            fanout_admission: spawner.fanout_parent("parent-run"),
            reply_obligations: Arc::new(Default::default()),
            inherited_permissions: InheritedPermissions::auto_approve(),
            enabled_tools: None,
            active_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            execution_metadata: None,
            execution_deadline: None,
            workspace_mutation: astra_runtime::orchestration::WorkspaceMutationAuthority::default(),
            transcript_location:
                astra_runtime::orchestration::AgentTranscriptLocation::LocalJournal,
        };

        let launch: Value = serde_json::from_str(
            &astra_runtime::orchestration::handle_agent_spawn_action(
                &json!({
                    "action": "spawn",
                    "description": "Inherited Offering child",
                    "prompt": "Return one completed child result.",
                    "agent_type": "general-purpose"
                }),
                Some(&context),
            )
            .await,
        )
        .expect("public spawn handler must return a JSON launch receipt");
        assert_eq!(launch["status"], "launched", "{launch}");
        assert_eq!(
            launch["prepared_model"],
            json!({
                "model_name": "parent-model",
                "provenance": "inherited_parent_context"
            }),
            "launch must retain the inherited prepared model provenance"
        );

        let agent_id = launch["agent_id"]
            .as_str()
            .expect("launch receipt must contain the generated child agent id");
        let status = spawner
            .wait_for_agent(agent_id, std::time::Duration::from_secs(5))
            .await
            .expect("spawned child must reach a terminal state");
        assert!(
            matches!(
                status,
                astra_runtime::orchestration::AgentStatus::Completed { .. }
            ),
            "child status: {status:?}"
        );

        let state = spawner
            .get_agent_state_any(agent_id)
            .await
            .expect("completed child state must remain inspectable");
        let prepared = state
            .prepared_model
            .expect("child state must retain the prepared model identity");
        assert_eq!(prepared.offering_id, "offer-parent");
        assert_eq!(prepared.model_name, "parent-model");
        assert_eq!(prepared.provenance, "inherited_parent_context");

        // MockLlmServer records inference bodies only and has no
        // /model-access/admit route: an unexpected admission request returns
        // 404 and prevents this child from reaching Completed. Keep the
        // request count below explicitly scoped to inference.
        let inference_requests = mock.received_requests();
        assert_eq!(
            inference_requests.len(),
            1,
            "the child must make one inference request; an unexpected model-access admit would return 404 and prevent completion"
        );
        assert_eq!(
            inference_requests[0]["model_selection"]["offering_id"],
            "offer-parent"
        );
    }

    #[tokio::test]
    async fn cli_fanout_preadmits_mixed_inherited_and_explicit_models_atomically() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "slots": [
                    {"offering_id":"offer-flash","reasoning":{"mode":"model_default"},"model_name":"deepseek-v4-flash","context_window":128000},
                    {"offering_id":"offer-glm","reasoning":{"mode":"adaptive","effort":"high"},"model_name":"glm-5.2","context_window":200000}
                ]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let executor = Arc::new(test_executor(&server.uri()));
        let context = cli_fanout_test_context();
        let inputs = [
            SpawnAgentInput {
                description: "flash review".into(),
                prompt: "review".into(),
                fanout_group_id: Some("review".into()),
                fanout_target_count: Some(3),
                fanout_slot_index: Some(0),
                ..Default::default()
            },
            SpawnAgentInput {
                description: "glm review".into(),
                prompt: "review".into(),
                requested_model_policy: Some(astra_turn_types::RequestedModelPolicy::Fixed {
                    selector: astra_turn_types::ModelSelector::ConfiguredName {
                        model_name: "glm-5.2".into(),
                        source: None,
                    },
                }),
                reasoning: Some(
                    astra_turn_core::orchestration_spawn_tool::ReasoningSelection::Adaptive {
                        effort: astra_turn_core::thinking_config::ThinkingEffort::High,
                    },
                ),
                fanout_group_id: Some("review".into()),
                fanout_target_count: Some(3),
                fanout_slot_index: Some(1),
                ..Default::default()
            },
            SpawnAgentInput {
                description: "glm second review".into(),
                prompt: "review".into(),
                requested_model_policy: Some(astra_turn_types::RequestedModelPolicy::Fixed {
                    selector: astra_turn_types::ModelSelector::ConfiguredName {
                        model_name: "glm-5.2".into(),
                        source: None,
                    },
                }),
                reasoning: Some(
                    astra_turn_core::orchestration_spawn_tool::ReasoningSelection::Adaptive {
                        effort: astra_turn_core::thinking_config::ThinkingEffort::High,
                    },
                ),
                fanout_group_id: Some("review".into()),
                fanout_target_count: Some(3),
                fanout_slot_index: Some(2),
                ..Default::default()
            },
        ];
        let parent = astra_turn_types::ModelSelection {
            offering_id: "offer-flash".into(),
        };
        let prepared = Arc::clone(&executor)
            .prepare_batch(&inputs, &context, Some(&parent))
            .await
            .expect("all child slots admitted before launch");
        assert_eq!(prepared.len(), 3);
        assert_eq!(
            prepared[0].model_identity().unwrap().offering_id,
            "offer-flash"
        );
        assert_eq!(
            prepared[1].model_identity().unwrap().offering_id,
            "offer-glm"
        );
        assert_eq!(
            prepared[1].model_identity().unwrap().provenance,
            "admission_validated"
        );
        assert_eq!(
            prepared[2].model_identity().unwrap().offering_id,
            "offer-glm"
        );
        for (prepared, input) in prepared.into_iter().zip(&inputs) {
            let admitted_selection =
                prepared
                    .model_identity()
                    .map(|identity| astra_turn_types::ModelSelection {
                        offering_id: identity.offering_id,
                    });
            let requested_selection = astra_turn_types::resolve_requested_model_selection(
                input.requested_model_policy.as_ref(),
                Some(&parent),
            )
            .unwrap_or(admitted_selection);
            let result = execute_prepared_cli_for_test(
                prepared,
                prepared_cli_test_config_for_input(
                    input,
                    requested_selection,
                    input
                        .reasoning
                        .as_ref()
                        .map(astra_turn_core::orchestration_spawn_tool::ReasoningSelection::config)
                        .unwrap_or(astra_turn_core::thinking_config::ThinkingConfig::ModelDefault),
                ),
            )
            .await
            .expect_err("valid binding reaches the non-network turn-limit guard");
            assert!(
                result.contains("hard_turn_limit must be positive"),
                "{result}"
            );
        }
        let mut explicit_inputs = inputs.clone();
        explicit_inputs[0].requested_model_policy =
            Some(astra_turn_types::RequestedModelPolicy::Fixed {
                selector: astra_turn_types::ModelSelector::OfferingId {
                    offering_id: parent.offering_id.clone(),
                },
            });
        let explicit = Arc::clone(&executor)
            .prepare_batch(&explicit_inputs, &context, None)
            .await
            .expect("all-explicit slots need no parent lookup");
        assert_eq!(explicit.len(), 3);
        server.verify().await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one admission POST per group; no catalog GET or child request"
        );
        assert_eq!(
            requests[0].body_json::<Value>().unwrap()["slots"][1]["selector"],
            json!({"kind":"configured_name","model_name":"glm-5.2"})
        );

        let mismatch_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "slots": [
                    {"offering_id":"offer-flash","reasoning":{"mode":"model_default"},"model_name":"deepseek-v4-flash","context_window":128000},
                    {"offering_id":"offer-other","reasoning":{"mode":"adaptive","effort":"high"},"model_name":"glm-5.2","context_window":200000}
                ]
            })))
            .expect(1)
            .mount(&mismatch_server)
            .await;
        let mismatch_executor = Arc::new(test_executor(&mismatch_server.uri()));
        let mut mismatched_inputs = inputs.clone();
        mismatched_inputs[1].requested_model_policy =
            Some(astra_turn_types::RequestedModelPolicy::Fixed {
                selector: astra_turn_types::ModelSelector::OfferingId {
                    offering_id: "offer-glm".into(),
                },
            });
        assert!(
            Arc::clone(&mismatch_executor)
                .prepare_batch(&mismatched_inputs, &context, Some(&parent))
                .await
                .is_err()
        );
        mismatch_server.verify().await;
        for status in [401, 503] {
            let failure_server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/model-access/admit"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&failure_server)
                .await;
            let failure_executor = Arc::new(test_executor(&failure_server.uri()));
            assert!(
                Arc::clone(&failure_executor)
                    .prepare_batch(&inputs, &context, Some(&parent))
                    .await
                    .is_err(),
                "HTTP {status} must not prepare any slot"
            );
            failure_server.verify().await;
            assert_eq!(failure_server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn configured_name_inherits_parent_reasoning_through_one_batch_admission() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "slots": [{
                    "offering_id":"offer-glm",
                    "reasoning":{"mode":"adaptive","effort":"high"},
                    "model_name":"glm-5.2",
                    "context_window":128000
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let executor = Arc::new(test_executor(&server.uri()));
        let mut context = cli_fanout_test_context();
        context.parent_model_reasoning = Some(
            astra_turn_core::orchestration_spawn_tool::ParentModelReasoning {
                selection: astra_turn_types::ModelSelection {
                    offering_id: "offer-glm".into(),
                },
                resolved_model_name: Some("glm-5.2".into()),
                thinking: astra_turn_core::thinking_config::ThinkingConfig::Adaptive {
                    effort: astra_turn_core::thinking_config::ThinkingEffort::High,
                },
            },
        );
        let input = SpawnAgentInput {
            description: "named inherited child".into(),
            prompt: "reply".into(),
            requested_model_policy: Some(astra_turn_types::RequestedModelPolicy::Fixed {
                selector: astra_turn_types::ModelSelector::ConfiguredName {
                    model_name: "glm-5.2".into(),
                    source: None,
                },
            }),
            fanout_group_id: Some("group".into()),
            fanout_target_count: Some(1),
            fanout_slot_index: Some(0),
            ..Default::default()
        };
        let prepared = Arc::clone(&executor)
            .prepare_batch(std::slice::from_ref(&input), &context, None)
            .await
            .expect("configured name is admitted before launch");
        assert_eq!(prepared.len(), 1);
        let identity = prepared[0].model_identity().expect("prepared identity");
        assert_eq!(identity.offering_id, "offer-glm");
        assert_eq!(identity.model_name, "glm-5.2");
        let result = execute_prepared_cli_for_test(
            prepared.into_iter().next().unwrap(),
            prepared_cli_test_config_for_input(
                &input,
                Some(astra_turn_types::ModelSelection {
                    offering_id: "offer-glm".into(),
                }),
                astra_turn_core::thinking_config::ThinkingConfig::Adaptive {
                    effort: astra_turn_core::thinking_config::ThinkingEffort::High,
                },
            ),
        )
        .await
        .expect_err("trusted binding reaches the deliberate test turn-limit guard");
        assert!(
            result.contains("hard_turn_limit must be positive"),
            "{result}"
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "name resolution and admission are one call"
        );
        let body = requests[0].body_json::<Value>().unwrap();
        assert_eq!(
            body["slots"][0]["selector"],
            json!({"kind":"configured_name","model_name":"glm-5.2"})
        );
        assert_eq!(
            body["slots"][0]["inherited_reasoning"],
            json!({
                "offering_id":"offer-glm",
                "reasoning":{"mode":"adaptive","effort":"high"}
            })
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn cli_fanout_consumes_inherited_model_with_explicit_reasoning() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model-access/admit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "slots": [{
                    "offering_id":"offer-parent",
                    "reasoning":{"mode":"off"},
                    "model_name":"parent-model",
                    "context_window":128000
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let executor = Arc::new(test_executor(&server.uri()));
        let input = SpawnAgentInput {
            description: "reasoning override".into(),
            prompt: "reply".into(),
            reasoning: Some(astra_turn_core::orchestration_spawn_tool::ReasoningSelection::Off),
            fanout_group_id: Some("group".into()),
            fanout_target_count: Some(1),
            fanout_slot_index: Some(0),
            ..Default::default()
        };
        let parent = astra_turn_types::ModelSelection {
            offering_id: "offer-parent".into(),
        };
        let prepared = Arc::clone(&executor)
            .prepare_batch(
                std::slice::from_ref(&input),
                &cli_fanout_test_context(),
                Some(&parent),
            )
            .await
            .expect("reasoning-only slot admitted");
        let result = execute_prepared_cli_for_test(
            prepared.into_iter().next().unwrap(),
            prepared_cli_test_config(
                input.fanout_slot_identity().unwrap(),
                Some(parent),
                astra_turn_core::thinking_config::ThinkingConfig::Off,
            ),
        )
        .await
        .expect_err("valid binding reaches the non-network turn-limit guard");
        assert!(
            result.contains("hard_turn_limit must be positive"),
            "{result}"
        );
        server.verify().await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cli_fanout_inherits_only_same_offering_effective_reasoning() {
        use astra_turn_core::orchestration_spawn_tool::{ParentModelReasoning, ReasoningSelection};
        use astra_turn_core::thinking_config::{ThinkingConfig, ThinkingEffort};

        for inherited in [
            ThinkingConfig::Off,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
            ThinkingConfig::Enabled {
                budget_tokens: 4096,
            },
        ] {
            let server = MockServer::start().await;
            let parent = astra_turn_types::ModelSelection {
                offering_id: "offer-parent".into(),
            };
            let other = astra_turn_types::ModelSelection {
                offering_id: "offer-other".into(),
            };
            let context = SpawnContext {
                parent_model_reasoning: Some(ParentModelReasoning {
                    selection: parent.clone(),
                    resolved_model_name: Some("parent-model".into()),
                    thinking: inherited.clone(),
                }),
                ..cli_fanout_test_context()
            };
            let inputs: Vec<_> = [
                (None, None),
                (Some(parent.clone()), None),
                (Some(other.clone()), None),
                (None, Some(ReasoningSelection::ModelDefault)),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (model_selection, reasoning))| SpawnAgentInput {
                description: format!("slot {index}"),
                prompt: "review".into(),
                requested_model_policy: model_selection.map(|selection| {
                    astra_turn_types::RequestedModelPolicy::Fixed {
                        selector: astra_turn_types::ModelSelector::OfferingId {
                            offering_id: selection.offering_id,
                        },
                    }
                }),
                reasoning,
                fanout_group_id: Some("reasoning".into()),
                fanout_target_count: Some(4),
                fanout_slot_index: Some(index),
                ..Default::default()
            })
            .collect();
            let effective = [
                inherited.clone(),
                inherited.clone(),
                ThinkingConfig::ModelDefault,
                ThinkingConfig::ModelDefault,
            ];
            let slot_indexes = [0, 0, 1, 2];
            let representatives = [0, 2, 3];
            let slots: Vec<_> = representatives
                .into_iter()
                .map(|index| {
                    let input = &inputs[index];
                    let thinking = &effective[index];
                    json!({
                        "offering_id": astra_turn_types::resolve_requested_model_selection(
                            input.requested_model_policy.as_ref(), Some(&parent)
                        ).unwrap().unwrap().offering_id,
                        "reasoning": ReasoningSelection::from(thinking.clone()),
                        "model_name": "admitted-model",
                        "context_window": 128000,
                    })
                })
                .collect();
            Mock::given(method("POST"))
                .and(path("/model-access/admit"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"slots": slots})))
                .expect(1)
                .mount(&server)
                .await;
            let executor = Arc::new(test_executor(&server.uri()));
            let prepared = executor
                .prepare_batch(&inputs, &context, Some(&parent))
                .await
                .unwrap();
            assert_eq!(prepared.len(), 4);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1, "no catalog GET before admission");
            let body = requests[0].body_json::<Value>().unwrap();
            for (index, ((prepared, input), thinking)) in
                prepared.into_iter().zip(&inputs).zip(effective).enumerate()
            {
                assert_eq!(
                    body["slots"][slot_indexes[index]]["reasoning"],
                    serde_json::to_value(ReasoningSelection::from(thinking.clone())).unwrap()
                );
                let error = execute_prepared_cli_for_test(
                    prepared,
                    prepared_cli_test_config_for_input(
                        input,
                        astra_turn_types::resolve_requested_model_selection(
                            input.requested_model_policy.as_ref(),
                            Some(&parent),
                        )
                        .unwrap(),
                        thinking,
                    ),
                )
                .await
                .unwrap_err();
                assert!(
                    error.contains("hard_turn_limit must be positive"),
                    "{error}"
                );
            }
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn cli_fanout_inherited_budget_requires_exact_admission_and_consumption() {
        use astra_turn_core::orchestration_spawn_tool::ParentModelReasoning;
        use astra_turn_core::thinking_config::ThinkingConfig;
        let parent = astra_turn_types::ModelSelection {
            offering_id: "offer-parent".into(),
        };
        let input = SpawnAgentInput {
            description: "review".into(),
            prompt: "review".into(),
            max_output_tokens: Some(8192),
            ..Default::default()
        };
        let context = SpawnContext {
            parent_model_reasoning: Some(ParentModelReasoning {
                selection: parent.clone(),
                resolved_model_name: Some("parent-model".into()),
                thinking: ThinkingConfig::Enabled {
                    budget_tokens: 4096,
                },
            }),
            ..cli_fanout_test_context()
        };
        for (status, admitted_budget, admitted_cap, consumed_budget, consumed_cap) in [
            (400, 4096, Some(8192), 4096, Some(8192)),
            (200, 1024, Some(8192), 4096, Some(8192)),
            (200, 4096, None, 4096, Some(8192)),
            (200, 4096, Some(4096), 4096, Some(8192)),
            (200, 4096, Some(8192), 1024, Some(8192)),
            (200, 4096, Some(8192), 4096, None),
            (200, 4096, Some(8192), 4096, Some(8192)),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/model-access/admit"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({"slots": [{
                    "offering_id": "offer-parent", "reasoning": {"mode": "enabled", "budget_tokens": admitted_budget},
                    "model_name": "parent-model", "context_window": 128000,
                    "max_output_tokens": admitted_cap,
                }]})))
                .expect(1).mount(&server).await;
            let result = Arc::new(test_executor(&server.uri()))
                .prepare_batch(std::slice::from_ref(&input), &context, None)
                .await;
            if status != 200 || admitted_budget != 4096 || admitted_cap != Some(8192) {
                assert!(
                    result.is_err(),
                    "rejection/mismatched budget must not prepare a child"
                );
            } else {
                let prepared = result.unwrap().pop().unwrap();
                let mut config = prepared_cli_test_config(
                    None,
                    Some(parent.clone()),
                    ThinkingConfig::Enabled {
                        budget_tokens: consumed_budget,
                    },
                );
                config.max_output_tokens = consumed_cap;
                let error = execute_prepared_cli_for_test(prepared, config)
                    .await
                    .unwrap_err();
                if consumed_budget == 4096 && consumed_cap == Some(8192) {
                    assert!(
                        error.contains("hard_turn_limit must be positive"),
                        "{error}"
                    );
                } else {
                    assert!(error.contains("does not match"), "{error}");
                }
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(
                requests.len(),
                1,
                "inherited nondefault uses admission instead of catalog"
            );
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["slots"][0]["reasoning"],
                json!({"mode":"enabled", "budget_tokens":4096})
            );
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["slots"][0]["max_output_tokens"],
                8192
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn cli_fanout_without_typed_parent_selection_ignores_stale_display_name() {
        let server = MockServer::start().await;
        let offering = test_offering("default-offer", "default-model");
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [offering],
                "next_cursor": null, "limit": 50, "total": 1,
                "catalog_revision": "sha256:default"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let executor =
            Arc::new(test_executor(&server.uri()).with_default_model(Some("default-model".into())));
        let context = SpawnContext {
            resolved_model_name: Some("stale-display-name".into()),
            ..cli_fanout_test_context()
        };
        let inputs = vec![SpawnAgentInput {
            description: "review".into(),
            prompt: "review".into(),
            ..Default::default()
        }];
        let prepared = Arc::clone(&executor)
            .prepare_batch(&inputs, &context, None)
            .await
            .unwrap();
        let identity = prepared[0].model_identity().unwrap();
        assert_eq!(identity.offering_id, "default-offer");
        assert_eq!(identity.provenance, "catalog_resolved");
        server.verify().await;
        let mixed = [
            SpawnAgentInput {
                fanout_group_id: Some("mixed".into()),
                fanout_target_count: Some(2),
                fanout_slot_index: Some(0),
                ..inputs[0].clone()
            },
            SpawnAgentInput {
                description: "explicit".into(),
                prompt: "reply".into(),
                requested_model_policy: Some(astra_turn_types::RequestedModelPolicy::Fixed {
                    selector: astra_turn_types::ModelSelector::OfferingId {
                        offering_id: "explicit-offer".into(),
                    },
                }),
                fanout_group_id: Some("mixed".into()),
                fanout_target_count: Some(2),
                fanout_slot_index: Some(1),
                ..Default::default()
            },
        ];
        let error = match Arc::clone(&executor)
            .prepare_batch(&mixed, &context, None)
            .await
        {
            Ok(_) => panic!("untyped inherited slot cannot join an overridden fanout"),
            Err(error) => error,
        };
        assert!(error.contains("exact parent Offering"), "{error}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn cancelled_loop_projection_requires_typed_user_interruption() {
        assert_eq!(
            cancelled_loop_origin(Some(InterruptionKind::UserCancelled)),
            CancellationOrigin::User
        );
        assert_eq!(cancelled_loop_origin(None), CancellationOrigin::Runtime);
        assert_eq!(
            cancelled_loop_origin(Some(InterruptionKind::ExecutionIncomplete)),
            CancellationOrigin::Runtime
        );
    }

    #[test]
    fn classified_cancelled_error_projects_runtime_but_provider_error_remains_failure() {
        assert_eq!(
            classified_error_cancellation_origin(astra_core::ErrorKind::Cancelled),
            Some(CancellationOrigin::Runtime)
        );
        assert_eq!(
            classified_error_cancellation_origin(astra_core::ErrorKind::ServerError),
            None,
            "a real provider failure must remain on the failed branch"
        );
    }

    #[test]
    fn fresh_child_system_prompt_is_stable_across_run_identities() {
        let prompt = build_child_system_prompt("\nYou are an exploration agent.");
        assert!(prompt.starts_with("You are a specialized sub-agent."));
        assert!(prompt.contains("You are an exploration agent."));
        assert!(!prompt.contains("run_id"));
        assert!(!prompt.contains("agent_id"));
    }

    fn test_permission_context() -> (
        InheritedPermissions,
        astra_runtime::orchestration::PermissionSyncHandle,
    ) {
        let inherited_permissions = InheritedPermissions::new(PermissionMode::Prompt);
        let permission_context = PermissionSyncContext::shared(inherited_permissions.clone());
        (inherited_permissions, permission_context)
    }

    #[derive(Debug, Default)]
    struct RecordingLiveSink {
        events: std::sync::Mutex<Vec<AgentLiveEvent>>,
        gaps: std::sync::Mutex<Vec<astra_turn_core::agent_live_event::AgentLiveGap>>,
    }

    impl AgentLiveEventSink for RecordingLiveSink {
        fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
            self.events.lock_recover().push(event);
            Ok(())
        }

        fn send_gap(
            &self,
            gap: astra_turn_core::agent_live_event::AgentLiveGap,
        ) -> Result<(), AgentLiveSendError> {
            self.gaps.lock_recover().push(gap);
            Ok(())
        }
    }

    #[test]
    fn test_executor_creation() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor =
            CliSpawnAgentExecutor::new(api, "token".to_string(), PathBuf::from("/tmp"), None);
        assert!(executor.skill_resolver.is_none());
        assert!(
            executor.token_provider.is_none(),
            "fresh executor must have no provider — production wires \
             one via with_token_provider"
        );
        assert!(executor.default_model.is_none());
        let (session_id, journal) = executor.session_transcript_snapshot();
        assert!(session_id.is_none());
        assert!(journal.is_none());
    }

    #[test]
    fn session_transcript_binding_installs_matching_journal_and_identity() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let session_id = format!("spawn-transcript-{}", uuid::Uuid::new_v4());
        let executor =
            CliSpawnAgentExecutor::new(api, "token".to_string(), PathBuf::from("/tmp"), None)
                .with_session_transcript(session_id.clone());

        let (active_session_id, journal) = executor.session_transcript_snapshot();
        assert_eq!(active_session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            journal.as_ref().map(|journal| journal.path()),
            Some(&astra_services::session_journal::journal_file_path(
                &session_id
            ))
        );
    }

    #[test]
    fn late_session_transcript_binding_installs_matching_journal_and_identity() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let session_id = format!("spawn-transcript-late-{}", uuid::Uuid::new_v4());
        let executor =
            CliSpawnAgentExecutor::new(api, "token".to_string(), PathBuf::from("/tmp"), None);

        executor.bind_parent_session(&session_id);

        let (active_session_id, journal) = executor.session_transcript_snapshot();
        assert_eq!(active_session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            journal.as_ref().map(|journal| journal.path()),
            Some(&astra_services::session_journal::journal_file_path(
                &session_id
            ))
        );
    }

    #[test]
    fn nested_live_events_preserve_execution_identity_through_child_stream() {
        let live_sink = Arc::new(RecordingLiveSink::default());
        let stream_sink = agent_live_stream_event_sink(
            "child-run".into(),
            "child-agent".into(),
            Some(live_sink.clone()),
        )
        .expect("sink");
        stream_sink.send(StreamEvent::Token {
            model_item_id: None,
            text: "CHILD-MARKER".into(),
        });
        stream_sink.send(StreamEvent::AgentLive(
            astra_turn_core::agent_live_event::AgentLiveEvent {
                run_id: "grandchild-run".into(),
                agent_id: "grandchild-agent".into(),
                kind: AgentLiveEventKind::OutputDelta {
                    model_item_id: Some("test-model-item".into()),
                    text: "GRANDCHILD-MARKER".into(),
                },
            },
        ));
        let events = live_sink.events.lock_recover();
        assert_eq!(events.len(), 2, "nested execution output must reach the UI");
        assert_eq!(events[0].run_id, "child-run");
        assert_eq!(events[0].agent_id, "child-agent");
        assert_eq!(events[1].run_id, "grandchild-run");
        assert_eq!(events[1].agent_id, "grandchild-agent");
        assert!(
            matches!(&events[1].kind, AgentLiveEventKind::OutputDelta { text, .. }
            if text == "GRANDCHILD-MARKER")
        );
    }

    #[test]
    fn agent_live_stream_event_sink_translates_directly_without_stream_channel() {
        let live_sink = Arc::new(RecordingLiveSink::default());
        let stream_sink = agent_live_stream_event_sink(
            "run-reviewer-1".into(),
            "reviewer@abc12345".into(),
            Some(live_sink.clone()),
        )
        .expect("sink");

        stream_sink.send(StreamEvent::Token {
            model_item_id: None,
            text: "hello".into(),
        });
        stream_sink.send(StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "cargo test".into(),
            tool_use_id: "tool-1".into(),
            parent_tool_use_id: None,
        });
        stream_sink.send(StreamEvent::PermissionAutoApproved {
            tool: "bash".into(),
            reason: "session rule".into(),
        });

        let events = live_sink.events.lock_recover();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0].kind,
            AgentLiveEventKind::OutputDelta { .. }
        ));
        assert!(matches!(
            events[1].kind,
            AgentLiveEventKind::ToolStarted { .. }
        ));

        stream_sink.send(StreamEvent::AgentLiveGap(
            astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "run-reviewer-1".into(),
                agent_id: "reviewer@abc12345".into(),
                dropped_event_count: 2,
            },
        ));
        assert_eq!(
            live_sink.gaps.lock_recover().as_slice(),
            [astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "run-reviewer-1".into(),
                agent_id: "reviewer@abc12345".into(),
                dropped_event_count: 2,
            }],
            "nested stream gaps must retain their typed repair semantics"
        );
        assert_eq!(events[0].agent_id, "reviewer@abc12345");
        assert!(matches!(
            &events[2].kind,
            AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::PermissionAutoApproved {
                    tool,
                    reason,
                }
            ) if tool == "bash" && reason == "session rule"
        ));
    }

    #[test]
    fn local_transcript_commit_is_emitted_as_typed_identity() {
        let live_sink = Arc::new(RecordingLiveSink::default());
        let sink: SharedAgentLiveEventSink = live_sink.clone();
        emit_agent_transcript_committed(
            Some(&sink),
            "run-reviewer-1",
            "reviewer@abc12345",
            "local-transcript:sha256:abc".into(),
            astra_turn_types::AgentTranscriptLocation::LocalJournal,
        );

        let events = live_sink.events.lock_recover();
        assert!(matches!(
            events.as_slice(),
            [AgentLiveEvent {
                run_id,
                agent_id,
                kind: AgentLiveEventKind::Signal(
                    astra_turn_core::agent_live_event::AgentLiveSignal::TranscriptCommitted {
                        source_event_id,
                        transcript_location:
                            astra_turn_types::AgentTranscriptLocation::LocalJournal,
                        ..
                    }
                ),
            }] if run_id == "run-reviewer-1"
                && agent_id == "reviewer@abc12345"
                && source_event_id == "local-transcript:sha256:abc"
        ));
    }

    /// REGRESSION (session 82ff91e5): sub-agent spawns failed with
    /// "Could not validate credentials" because the executor froze
    /// `token: String` at construction time and never refreshed it.
    /// Long-running interactive sessions rotate the token via the
    /// parent's 401-refresh-and-retry path; without a provider, the
    /// stale token stays in the spawn executor and every spawn 401s.
    ///
    /// This test pins the fix: when a token provider is installed,
    /// `resolve_token_async()` MUST return the provider's value, not the
    /// frozen one. Mutating the provider's source between calls
    /// proves freshness.
    #[tokio::test]
    async fn token_provider_overrides_stale_captured_token() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor_no_provider = CliSpawnAgentExecutor::new(
            api.clone(),
            "stale-token".to_string(),
            PathBuf::from("/tmp"),
            None,
        );
        assert_eq!(
            executor_no_provider.resolve_token_async().await.unwrap(),
            "stale-token",
            "without a provider, fall back to the captured token"
        );

        // Provider returns whatever the shared mutable cell currently holds.
        let live_token = std::sync::Arc::new(std::sync::Mutex::new("v1".to_string()));
        let live_token_for_closure = live_token.clone();
        let provider: TokenProvider =
            std::sync::Arc::new(move || Some(live_token_for_closure.lock_recover().clone()));
        let executor = CliSpawnAgentExecutor::new(
            api,
            "stale-frozen-fallback".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .with_token_provider(provider);

        assert_eq!(
            executor.resolve_token_async().await.unwrap(),
            "v1",
            "provider must take precedence over the captured fallback"
        );
        // Simulate a token refresh in the parent flow.
        *live_token.lock_recover() = "v2-refreshed".to_string();
        assert_eq!(
            executor.resolve_token_async().await.unwrap(),
            "v2-refreshed",
            "subsequent spawns must read the refreshed token, not a frozen copy"
        );
    }

    /// Defensive: when the provider returns `None` (e.g. user logged
    /// out mid-session), fall back to the captured token rather than
    /// crashing or sending an empty string. The captured token will
    /// itself fail with 401 — but at least with a recognisable error.
    #[tokio::test]
    async fn token_provider_none_falls_back_to_captured() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| None);
        let executor = CliSpawnAgentExecutor::new(
            api,
            "fallback-token".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .with_token_provider(provider);

        assert_eq!(
            executor.resolve_token_async().await.unwrap(),
            "fallback-token",
            "provider returning None must fall back to the captured token"
        );
    }

    #[tokio::test]
    async fn async_token_provider_panic_surfaces_instead_of_using_stale_fallback() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| panic!("token store poisoned"));
        let executor =
            CliSpawnAgentExecutor::new(api, "stale-token".to_string(), PathBuf::from("/tmp"), None)
                .with_token_provider(provider);

        let err = executor.resolve_token_async().await.unwrap_err();
        assert!(
            err.contains("token provider task failed"),
            "join errors must be surfaced, got {err}"
        );
    }

    #[tokio::test]
    async fn cli_single_spawn_reuses_parent_offering_and_sends_exact_identity() {
        let mock =
            crate::cli::mock_llm::MockLlmServer::start(crate::cli::mock_llm::MockScenario::Fail)
                .await
                .expect("mock Server");
        let executor = test_executor(&mock.base_url);
        let offering = astra_turn_types::ModelSelection {
            offering_id: "offer-parent".into(),
        };
        let mut config = prepared_cli_test_config(
            None,
            Some(offering),
            astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
        );
        config.model = Some("parent-model".into());
        config.hard_turn_limit = Some(1);

        let _ = execute_cli_for_test(Arc::new(executor), config).await;
        let requests = mock.received_requests();
        assert_eq!(
            requests.len(),
            1,
            "the child reaches its first model request"
        );
        assert_eq!(
            requests[0]["model_selection"]["offering_id"], "offer-parent",
            "Server must receive the exact parent Offering and perform fresh authorization"
        );
    }

    #[tokio::test]
    async fn token_resolution_failure_preserves_the_local_transcript_binding() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| panic!("token store poisoned"));
        let live_sink = Arc::new(RecordingLiveSink::default());
        let executor =
            CliSpawnAgentExecutor::new(api, "stale-token".to_string(), PathBuf::from("/tmp"), None)
                .with_token_provider(provider);

        let err = execute_cli_for_test(
            Arc::new(executor),
            SpawnRunConfig {
                run_id: "run-1".into(),
                cancellation_binding_id: "test-run-1-binding".into(),
                agent_id: "reviewer@panic".into(),
                spawn_tool_call_id: Some("spawn-call-1".into()),
                recursion_depth: 1,
                agent_type: "task".into(),
                description: "Review token failure".into(),
                task: "review".into(),
                model: Some("test-model".into()),
                resolved_model_selection: Some(astra_turn_types::ModelSelection {
                    offering_id: "offer-test-model".into(),
                }),
                hard_turn_limit: Some(1),
                parent_address: None,
                live_event_sink: Some(live_sink.clone()),
                ..prepared_cli_test_config(
                    None,
                    None,
                    astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
                )
            },
        )
        .await
        .expect_err("token provider panic should fail execute");

        assert!(err.contains("token provider task failed"), "{err}");
        let events = live_sink.events.lock_recover();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].kind,
            AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                    transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
                    ..
                }
            )
        ));
        assert!(matches!(
            events[1].kind,
            AgentLiveEventKind::AgentTerminated { .. }
        ));
    }

    #[tokio::test]
    async fn spawned_llm_output_reaches_the_live_sink_end_to_end() {
        let mock = crate::cli::mock_llm::MockLlmServer::start(
            crate::cli::mock_llm::MockScenario::Complete,
        )
        .await
        .expect("mock LLM");
        let api = astra_thin_client::ThinClient::new(&mock.base_url, None).expect("test api");
        let live_sink = Arc::new(RecordingLiveSink::default());
        let executor =
            CliSpawnAgentExecutor::new(api, "test-token".into(), std::env::temp_dir(), None);

        let result = execute_cli_for_test(
            Arc::new(executor),
            SpawnRunConfig {
                run_id: "run-live-output".into(),
                cancellation_binding_id: "test-run-live-output-binding".into(),
                agent_id: "reviewer@run-live-output".into(),
                spawn_tool_call_id: Some("call-spawn-live".into()),
                max_output_tokens: Some(32768),
                recursion_depth: 1,
                agent_type: "task".into(),
                description: "Live output child".into(),
                task: "Return one concise finding.".into(),
                model: Some("mock-model".into()),
                resolved_model_selection: Some(astra_turn_types::ModelSelection {
                    offering_id: "offer-mock-model".into(),
                }),
                hard_turn_limit: Some(1),
                isolated: false,
                working_dir: std::env::temp_dir(),
                parent_address: None,
                live_event_sink: Some(live_sink.clone()),
                ..prepared_cli_test_config(
                    None,
                    None,
                    astra_turn_core::thinking_config::ThinkingConfig::Enabled {
                        budget_tokens: 16384,
                    },
                )
            },
        )
        .await
        .expect("spawned run");
        assert_eq!(result.status, "completed");
        let requests = mock.received_requests();
        assert!(!requests.is_empty(), "child must send an actual request");
        assert_eq!(
            requests.len(),
            1,
            "a canonical Server terminal must not trigger client replay"
        );
        assert_eq!(requests[0]["context"]["max_output_tokens"], 32768);
        assert_eq!(
            requests[0]["context"]["thinking"],
            serde_json::to_value(astra_turn_core::thinking_config::ThinkingConfig::Enabled {
                budget_tokens: 16384
            })
            .unwrap(),
            "typed child budget must bypass lightweight-turn scaling"
        );
        let events = live_sink.events.lock_recover();
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                    spawn_tool_call_id: Some(tool_call_id),
                    ..
                }
            )) if tool_call_id == "call-spawn-live"
        ));
        assert!(
            events.iter().any(|event| matches!(
                &event.kind,
                AgentLiveEventKind::OutputDelta { text, .. } if !text.trim().is_empty()
            )),
            "events: {events:?}"
        );
    }

    #[tokio::test]
    async fn child_rejects_invalid_exact_budget_before_lookup_or_inference() {
        use astra_turn_core::thinking_config::ThinkingConfig;
        let server = MockServer::start().await;
        for (budget, cap) in [(512, 8192), (8192, 8192), (16384, 8192)] {
            let mut config = prepared_cli_test_config(
                None,
                None,
                ThinkingConfig::Enabled {
                    budget_tokens: budget,
                },
            );
            config.max_output_tokens = Some(cap);
            config.hard_turn_limit = Some(1);
            let error = execute_cli_for_test(Arc::new(test_executor(&server.uri())), config)
                .await
                .unwrap_err();
            assert_eq!(
                error,
                ThinkingConfig::Enabled {
                    budget_tokens: budget
                }
                .validate_output_budget(u64::from(cap))
                .unwrap_err()
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// Bug1 regression: when inherited prefix ends with a user or tool
    /// message, the child task (also user role) creates consecutive
    /// user messages. Bedrock Converse rejects this with HTTP 400:
    /// "The provided request is not valid".
    ///
    /// The fix must insert a synthetic assistant message between the
    /// prefix and the child task when the last prefix message has
    /// role "user" or "tool".
    #[test]
    fn prefix_ending_with_user_or_tool_must_insert_assistant_bridge() {
        // Simulate the message construction logic from execute()
        let prefix_messages = vec![json!({"role": "user", "content": "original prompt"})];
        let system_prompt = "You are a child agent.";
        let child_task = "Reply: inherited-ok";

        let messages = build_child_messages(
            system_prompt,
            Some(&prefix_messages),
            child_task,
            false,
            "child-run-1",
        );

        // After system, the messages should NOT have two consecutive user roles.
        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}] — \
                 Bedrock will reject with HTTP 400",
                window[0], window[1]
            );
        }
    }

    /// Same as above but for prefix ending with tool role.
    #[test]
    fn prefix_ending_with_tool_must_insert_assistant_bridge() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "do something"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
        ];
        let messages = build_child_messages(
            "system",
            Some(&prefix_messages),
            "child task",
            false,
            "child-run-1",
        );

        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}]",
                window[0], window[1]
            );
        }
    }

    /// When prefix ends with assistant, no bridge is needed.
    /// Fork mode: prefix is used verbatim (no system prepend).
    #[test]
    fn prefix_ending_with_assistant_needs_no_bridge() {
        let prefix_messages = vec![
            json!({"role": "system", "content": "parent system prompt"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let messages = build_child_messages(
            "ignored",
            Some(&prefix_messages),
            "child task",
            false,
            "child-run-1",
        );

        // Fork mode: prefix verbatim + child task. No bridge needed
        // because prefix ends with assistant → child task (user) is valid.
        let roles: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    }

    #[test]
    fn fork_mode_backfills_reasoning_fields_when_inherited_prefix_requires_them() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "do something"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "1", "function": {"name": "bash", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
        ];
        let messages = build_child_messages(
            "system",
            Some(&prefix_messages),
            "child task",
            true,
            "child-run-1",
        );
        assert_eq!(messages[1]["reasoning_content"], "");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["reasoning_content"], "");
    }

    #[test]
    fn child_bridge_identity_marks_only_the_current_task_suffix() {
        let mut inherited_user = json!({"role": "user", "content": "parent task"});
        assert!(astra_turn_types::mark_turn_message(
            &mut inherited_user,
            "parent-run"
        ));
        let prefix_messages = vec![
            inherited_user,
            json!({"role": "assistant", "content": "parent answer"}),
        ];

        let messages = build_child_messages(
            "ignored",
            Some(&prefix_messages),
            "child task",
            false,
            "child-run",
        );

        let provenances = messages
            .iter()
            .map(|message| {
                astra_turn_types::turn_message_provenance(message)
                    .expect("producer-owned provenance must be valid")
                    .map(|provenance| provenance.turn_chain_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            provenances,
            vec![Some("parent-run".into()), None, Some("child-run".into())]
        );
        assert_eq!(
            messages
                .last()
                .and_then(|message| message["content"].as_str()),
            Some("child task")
        );
    }
}
