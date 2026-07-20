//! CLI implementation of [`AgenticLoopHost`].
//!
//! Wraps CLI-specific concerns (tool executor, permission manager, tool surface,
//! skill registry, terminal rendering) behind the runtime trait so the
//! multi-turn loop runs in the runtime crate.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use astra_runtime::{
    tool_registry::ToolRegistry,
    turn::agentic::headless_round::HeadlessStderrStyle,
    turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, ControlToolRecovery, HostTurnResult,
        SkillAutoRouteDecision, SkillAutoRouteJudgeContext, TurnInteractionMode,
        interaction_scoped_tool_restrictions,
    },
};
use astra_turn_core::{
    chat_turn_sse_dispatch::ChatTurnSseAccum, compaction_types::CompactionEvent,
    orchestration::agent_result_wire::render_agent_tool_error, sse_stream_host::EdgeToolExecResult,
    tool::schema::tool_names_from_schemas, tool_result_semantics::cloud_tool_result_status_label,
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use serde_json::Value;

use crate::{
    ExplainMode,
    cli::permission_manager::{PermissionManager, PermissionMode},
    cli::stream::stream_render::{
        RenderPolicy, agent_control_action, agent_control_label, agent_id_from_args,
        agent_id_from_output, tool_output_event_text,
    },
    edge_tools::ToolExecutor,
};

use crate::cli::chat_stream::sse_loop::agentic_loop_turn::{
    ChatTurnSseFetchRequest, PrepareTurnTelemetry, fetch_chat_turn_sse,
};
use crate::cli::chat_stream::sse_loop::refresh_root_permission_context;

use astra_runtime::tool_sandbox::SandboxPolicy;

const AGENT_FANOUT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(3);

struct CliServerProxySummaryClient {
    api: astra_thin_client::ThinClient,
    token: String,
    base_scope: astra_turn_types::InferenceInvocationScope,
    next_logical_attempt: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl astra_turn_core::cloud_summary::SummaryLlmClient for CliServerProxySummaryClient {
    async fn summarize(
        &self,
        purpose: astra_turn_types::InferencePurpose,
        messages: &[Value],
    ) -> Result<astra_turn_core::cloud_summary::SummaryResponse, String> {
        let logical_attempt = self
            .next_logical_attempt
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let mut request = astra_thin_client::CompletionRequest::new(
            purpose,
            self.base_scope.with_logical_attempt(logical_attempt),
            messages.to_vec(),
        );
        request.max_tokens = 256;
        request.temperature = 0.0;
        let response = self
            .api
            .post_completions(&self.token, &request)
            .await
            .map_err(|error| error.to_string())?;
        let text = response
            .first_text()
            .ok_or_else(|| "Astra Server returned a completion without choices".to_string())?
            .to_string();
        Ok(astra_turn_core::cloud_summary::SummaryResponse {
            text,
            is_ptl_error: false,
        })
    }
}

struct CliSummaryClientSkillAutoRouteJudge {
    client: Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>,
}

#[async_trait]
impl astra_services::SkillAutoRouteJudge for CliSummaryClientSkillAutoRouteJudge {
    async fn judge(
        &self,
        ctx: &astra_services::SkillAutoRouteJudgeContext,
    ) -> Result<Option<String>, astra_services::SkillAutoRouteJudgeError> {
        let messages = astra_services::skill_auto_route_judge_messages(ctx)?;
        let allowed = ctx
            .visible_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();
        let response = self
            .client
            .summarize(astra_turn_types::InferencePurpose::Introspection, &messages)
            .await
            .map_err(astra_services::SkillAutoRouteJudgeError::Transport)?;
        astra_services::parse_skill_auto_route_response(response.text.as_str(), &allowed)
    }
}

fn cli_skill_auto_route_service_context(
    ctx: SkillAutoRouteJudgeContext<'_>,
) -> astra_services::SkillAutoRouteJudgeContext {
    astra_services::SkillAutoRouteJudgeContext {
        query: ctx.query.to_string(),
        visible_skills: ctx
            .visible_skills
            .iter()
            .map(|skill| astra_services::SkillAutoRouteCandidate {
                name: skill.name.clone(),
                description: skill.description.clone(),
                when_to_use: skill.when_to_use.clone(),
                aliases: skill.aliases.clone(),
            })
            .collect(),
    }
}

fn render_control_tool_recovery_error(message: &str) -> String {
    let mut value: Value = serde_json::from_str(&render_agent_tool_error(None, message))
        .unwrap_or_else(|_| serde_json::json!({"status": "failed", "error": message}));
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "recovery".to_string(),
            serde_json::json!({
                "attempted": true,
                "source": "host_state",
                "outcome": "failed",
            }),
        );
    }
    value.to_string()
}

const BRIDGE_SESSION_TURN_STALE_ERROR_CODE: &str = "bridge_session_turn_stale";

fn bridge_session_turn_stale_expected_turn(
    accum: &ChatTurnSseAccum,
    current_turn: u32,
) -> Option<u32> {
    if accum.error_message.is_none()
        || accum.error_code.as_deref() != Some(BRIDGE_SESSION_TURN_STALE_ERROR_CODE)
    {
        return None;
    }
    let metadata = accum.error_metadata.as_ref()?;
    let actual = metadata
        .get("actual_session_turn")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())?;
    let expected = metadata
        .get("expected_session_turn")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())?;
    if actual == current_turn && expected > current_turn {
        Some(expected)
    } else {
        None
    }
}

/// RAII guard for the executor's sandbox policy slot.
///
/// On drop, the slot is restored to whatever it held when [`SandboxPolicyGuard::install`]
/// was called — including the early-return path of `?` propagation, which was the
/// regression that motivated the guard. The previous shape (manual save / manual
/// restore) leaked the active policy into subsequent turns whenever `fetch_chat_turn_sse`
/// returned an `Err` before the restore line.
struct SandboxPolicyGuard<'a> {
    slot: &'a std::sync::RwLock<Option<SandboxPolicy>>,
    saved: Option<SandboxPolicy>,
}

impl<'a> SandboxPolicyGuard<'a> {
    /// Replace the slot with `next` (or restore the previous value if `next` is `None`)
    /// and return a guard that resets to whatever the slot held before the call when
    /// it goes out of scope.
    fn install(
        slot: &'a std::sync::RwLock<Option<SandboxPolicy>>,
        next: Option<SandboxPolicy>,
    ) -> Self {
        let mut write = astra_core::sync_poison::recover_rwlock_write(&slot);
        let saved = write.take();
        // If `next` is None we keep the previous policy (no skill activation this turn).
        // If `next` is Some, it replaces the previous policy for the duration of the guard.
        *write = next.or_else(|| saved.clone());
        drop(write);
        Self { slot, saved }
    }
}

impl Drop for SandboxPolicyGuard<'_> {
    fn drop(&mut self) {
        let mut write = astra_core::sync_poison::recover_rwlock_write(&self.slot);
        *write = self.saved.take();
    }
}

/// CLI host for the runtime agentic loop.
///
/// Holds all CLI-specific dependencies; the runtime loop calls `execute_turn()`
/// which delegates to the existing `fetch_chat_turn_sse` pipeline.
pub(crate) struct CliAgenticLoopHost<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: String,
    pub auth_profile: Option<&'a str>,
    pub offering_id: Option<String>,
    pub model: Option<&'a str>,
    pub context_window_tokens: u32,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub user_intent: &'a str,
    pub input_runtime_required_texts: &'a [String],
    pub input_active_system_skills: &'a [String],
    pub input_runtime_volatile_texts: &'a [String],
    pub semantic_query_override: Option<&'a str>,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: PathBuf,
    pub executor: Arc<ToolExecutor>,
    pub registry: ToolRegistry,
    pub all_schemas: Vec<Value>,
    pub file_context: Vec<String>,
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: HashSet<String>,
    pub capabilities: astra_turn_core::capability::CapabilitySet,
    /// Lines written to stderr between SSE turns (headless tool output, etc.)
    /// that the next `consume_turn_sse` must clear before streaming.
    pub pending_clear_lines: usize,
    pub is_plan_subtask: bool,
    pub plan_subtask_id: Option<&'a str>,
    pub plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Ordered control-state overflow for synchronous host callbacks. Textual
    /// progress may be sampled under pressure, but lifecycle start/finish
    /// pairs are drained before the terminal output boundary.
    pub pending_ordered_stream_events:
        std::collections::VecDeque<crate::cli::chat_stream::StreamEvent>,
    /// Request-scoped live lane for every child run, including `delegate`
    /// coordination. This is distinct from parent stream events so
    /// child activity cannot delay parent completion.
    pub agent_live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<crate::cli::chat_stream::ApprovalRequestTx>,
    /// Optional channel for native TUI ask_user prompts.
    pub ask_user_request_tx: Option<crate::cli::chat_stream::AskUserRequestTx>,
    /// Per-turn channel into the dedicated plan-review overlay used
    /// by `exit_plan_mode`. Lives next to `ask_user_request_tx` but
    /// stays separate so plan markdown does not have to be smuggled
    /// through the question/option layout `ask_user` expects.
    pub plan_review_request_tx: Option<crate::cli::chat_stream::PlanReviewRequestTx>,
    /// Root-level messaging context used when the current turn has no mailbox.
    pub root_send_message_context:
        Option<crate::edge_tools::agent_messaging::SendMessageRuntimeContext>,
    /// Shared dynamic-agent runtime for this session. It may be constructed
    /// before the server has allocated the session id, so the host late-binds it
    /// when the first streamed snapshot names the canonical session.
    pub agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// REPL turn counter (0-based) for correct turn_id in trace collector.
    pub chat_turn_index: u32,
    /// Cross-turn tool output cache for edge-path dedup.
    pub tool_cache: crate::cli::stream::stream_render::EdgeToolCache,
    /// Extra context appended to the system prompt via edge_profile.system_prompt_override.
    pub append_system_prompt: Option<String>,
    /// Optional fork-prefix store: when set + fork flag enabled,
    /// this host calls `capture_parent_prefix` in its
    /// `on_turn_completed` hook, feeding the store that the
    /// DynamicAgentSpawner and DelegationEngine share. Without
    /// this, a captured parent prefix never exists and children
    /// always resolve to None — which was the exact observation
    /// during live MiniMax verification (spawn succeeded,
    /// delegate succeeded, but fork-cache events never fired
    /// because no parent capture happened).
    pub prefix_store:
        Option<std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
    /// Incremental turn state for surviving interruptions.
    /// Written during streaming; snapped on force-exit to recover partial data.
    pub incremental_state: Option<Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
}

fn derive_turn_interaction_mode(
    permission_mode: PermissionMode,
    is_plan_subtask: bool,
    has_approval_request_tx: bool,
    has_ask_user_request_tx: bool,
    render_is_silent: bool,
    stdin_is_terminal: bool,
) -> TurnInteractionMode {
    // Plan subtasks are a structural override: a delegated subtask has
    // no user-facing session, so Auto/Prompt/Deny all collapse to
    // NonInteractive. Nothing the subtask does should depend on the
    // parent user's mode.
    if is_plan_subtask {
        return TurnInteractionMode::NonInteractive;
    }

    // Auto-resolving modes are the user's explicit opt-in to suppress
    // ordinary interaction nudges and ask_user prompts. Keep this as
    // TurnInteractionMode::Auto regardless of stdin / approval-channel /
    // silent render; hard permission gates are evaluated separately by
    // the permission engine and may still require review or deny.
    // Regression for session c6e18730 where piped or silent contexts
    // silently demoted Auto → NonInteractive, which disabled the
    // nudge-suppression gate the user opted into.
    if permission_mode.auto_resolves_approval_prompts() {
        return TurnInteractionMode::Auto;
    }

    match permission_mode {
        PermissionMode::Plan => {
            if has_approval_request_tx || render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else {
                TurnInteractionMode::Deny
            }
        }
        // AcceptEdits still needs the native ask_user sink for clarifications.
        // The old stdin/raw-mode path was removed because it corrupts the TUI
        // and has no product parity with the overlay flow.
        PermissionMode::AcceptEdits => {
            if render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else if has_ask_user_request_tx {
                TurnInteractionMode::Prompt
            } else {
                TurnInteractionMode::NonInteractive
            }
        }
        // Prompt also requires the native ask_user sink. Approval routing is
        // orthogonal here: if the session can surface questionnaire prompts,
        // keep ask_user available even when tool approvals are handled through
        // a separate channel.
        PermissionMode::Prompt => {
            if render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else if has_ask_user_request_tx {
                TurnInteractionMode::Prompt
            } else {
                TurnInteractionMode::NonInteractive
            }
        }
        // Deny under non-interactive contexts also collapses to
        // NonInteractive: there's nothing to refuse interactively and
        // Deny's deterministic-denial behaviour is already what
        // NonInteractive callers expect.
        PermissionMode::Deny => {
            if has_approval_request_tx || render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else {
                TurnInteractionMode::Deny
            }
        }
        mode => unreachable!("auto-resolving permission mode returned before match: {mode:?}"),
    }
}

impl CliAgenticLoopHost<'_> {
    /// Internal accessor. The trait impl delegates here; this method
    /// exists separately so other CLI-only call sites can use it
    /// without going through the `AgenticLoopHost` trait object.
    fn turn_interaction_mode_inherent(&self) -> TurnInteractionMode {
        derive_turn_interaction_mode(
            self.perm_manager.mode(),
            self.is_plan_subtask,
            self.approval_request_tx.is_some(),
            self.ask_user_request_tx.is_some(),
            self.render_policy.is_silent(),
            std::io::stdin().is_terminal(),
        )
    }

    /// Synchronous host callbacks cannot await UI backpressure. Stream events
    /// are observational (durable state lives elsewhere), so preserve bounded
    /// memory and make saturation visible instead of blocking a Tokio worker.
    fn try_emit_stream_event(&mut self, event: crate::cli::chat_stream::StreamEvent) {
        let Some(tx) = self.stream_event_tx.clone() else {
            return;
        };
        while let Some(pending) = self.pending_ordered_stream_events.pop_front() {
            match tx.try_send(pending) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(pending)) => {
                    self.pending_ordered_stream_events.push_front(pending);
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.pending_ordered_stream_events.clear();
                    return;
                }
            }
        }
        if !self.pending_ordered_stream_events.is_empty() {
            self.retain_ordered_stream_event(event);
            return;
        }
        match tx.try_send(event) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                self.retain_ordered_stream_event(event);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("stream event receiver closed");
            }
        }
    }

    fn retain_ordered_stream_event(&mut self, event: crate::cli::chat_stream::StreamEvent) {
        if !stream_event_requires_ordered_delivery(&event) {
            tracing::debug!("sampled non-stateful stream event under UI backpressure");
            return;
        }
        if self.pending_ordered_stream_events.len()
            >= crate::cli::chat_stream::STREAM_EVENT_CHANNEL_CAPACITY
        {
            tracing::error!(
                retained = self.pending_ordered_stream_events.len(),
                "ordered stream-event overflow exhausted; terminal state will reconcile at turn completion"
            );
            return;
        }
        self.pending_ordered_stream_events.push_back(event);
    }
}

fn stream_event_requires_ordered_delivery(event: &crate::cli::chat_stream::StreamEvent) -> bool {
    matches!(
        event,
        crate::cli::chat_stream::StreamEvent::ToolStarted { .. }
            | crate::cli::chat_stream::StreamEvent::ToolCompleted { .. }
            | crate::cli::chat_stream::StreamEvent::AgentControlStarted { .. }
            | crate::cli::chat_stream::StreamEvent::AgentControlCompleted { .. }
            | crate::cli::chat_stream::StreamEvent::AskUserPrompted { .. }
            | crate::cli::chat_stream::StreamEvent::AskUserResolved { .. }
            | crate::cli::chat_stream::StreamEvent::UserIntentApplied { .. }
    )
}

fn user_intent_stream_event(
    event: &astra_runtime::turn::run_control::QueuedUserIntent,
) -> Option<crate::cli::chat_stream::StreamEvent> {
    let content = astra_runtime::turn::run_control::user_intent_content(&event.input)?;
    Some(crate::cli::chat_stream::StreamEvent::UserIntentApplied {
        intent_id: event.intent_id.clone(),
        delivery: event.delivery,
        status: astra_turn_types::UserIntentStatus::Applied,
        event_index: event.event_index,
        content,
    })
}

async fn emit_final_output_ready(
    stream_event_tx: Option<&crate::cli::chat_stream::StreamEventTx>,
    pending_ordered: &mut std::collections::VecDeque<crate::cli::chat_stream::StreamEvent>,
) {
    let Some(tx) = stream_event_tx else {
        return;
    };
    while let Some(event) = pending_ordered.pop_front() {
        if let Err(error) = tx.send(event).await {
            tracing::debug!(%error, "ordered stream receiver closed during terminal drain");
            pending_ordered.clear();
            return;
        }
    }
    if let Err(error) = tx
        .send(crate::cli::chat_stream::StreamEvent::AssistantOutputSettled)
        .await
    {
        tracing::debug!(%error, "output-settled stream receiver closed");
    }
}

fn permission_mode_change_audit_event(
    session_id: Option<&str>,
    turn: u32,
    from_mode: PermissionMode,
    to_mode: PermissionMode,
    source: &str,
) -> astra_services::session_journal::JournalEvent {
    let mut event = astra_services::session_journal::JournalEvent::permission_audit(
        session_id,
        Some(turn),
        serde_json::json!({
            "kind": "permission_mode_changed",
            "from_mode": from_mode.to_string(),
            "to_mode": to_mode.to_string(),
            "source": source,
            "changed": from_mode != to_mode,
        }),
    );
    event.edge_policy = Some(astra_services::session_journal::EdgePolicySnapshot {
        permission_mode: Some(to_mode.to_string()),
        cloud_policy_version: None,
        rules_fingerprint: None,
    });
    event
}

fn append_permission_mode_change_audit(
    session_id: Option<&str>,
    turn: u32,
    from_mode: PermissionMode,
    to_mode: PermissionMode,
    source: &str,
) {
    let Some(session_id) = session_id.filter(|sid| !sid.trim().is_empty()) else {
        tracing::warn!(
            source,
            from_mode = %from_mode,
            to_mode = %to_mode,
            "permission mode changed without session_id; journal event skipped"
        );
        return;
    };
    let event =
        permission_mode_change_audit_event(Some(session_id), turn, from_mode, to_mode, source);
    match astra_services::session_journal::JournalWriter::new(session_id)
        .and_then(|writer| writer.append(&event))
    {
        Ok(()) => {}
        Err(error) => tracing::warn!(
            session_id,
            source,
            from_mode = %from_mode,
            to_mode = %to_mode,
            error = %error,
            "failed to journal permission mode change"
        ),
    }
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    fn memory_recall_scope(&self, _state: &AgenticLoopState) -> Option<(String, String)> {
        self.executor.memory_recall_scope()
    }

    fn agent_live_event_sink(
        &self,
    ) -> Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink> {
        self.agent_live_event_sink.clone()
    }

    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let assembly_start = Instant::now();

        // Preserve the lifecycle-created collector: it may already contain
        // turn setup trace data.
        let turn_id = format!("turn-{}", self.chat_turn_index);
        let session_id = state.current_session_id.clone().unwrap_or_default();
        if let Some(ref collector) = state.telemetry.turn_trace_collector {
            collector.set_turn_id(turn_id);
            collector.set_session_id(session_id);
        } else {
            state.telemetry.turn_trace_collector = Some(
                astra_runtime::turn::turn_trace_collector::TurnTraceCollector::new(
                    turn_id, session_id,
                ),
            );
        }
        let pre_clear = std::mem::take(&mut self.pending_clear_lines);

        // Apply any pending permission-mode change recorded by a tool
        // overlay during the previous turn (currently
        // `exit_plan_mode`'s 4-option dialog). Done at turn start
        // because mid-turn `set_mode` would race the agentic loop's
        // borrow of `perm_manager`. When a switch did happen we also
        // tell the model it now runs in the new mode so the next
        // round's reasoning is grounded in the new permission set.
        if let Some(new_mode) = self.executor.take_pending_permission_mode_change() {
            let old_mode = self.perm_manager.mode();
            self.perm_manager.set_mode(new_mode);
            refresh_root_permission_context(&mut state.permission_context, self.perm_manager).await;
            append_permission_mode_change_audit(
                state.current_session_id.as_deref(),
                self.chat_turn_index,
                old_mode,
                new_mode,
                "plan_approval_overlay",
            );
            state.push_volatile(
                astra_runtime::turn::agentic_loop::host::VolatileKind::PlanModeMarker,
                format!(
                    "[mode={new_mode}] User approved the plan; you are now executing in `{new_mode}` permission mode. Mutating tools are available — proceed to implement the plan."
                ),
            );
        }

        // Install the per-turn ask_user channel on the shared
        // ToolExecutor so tools that need a TUI overlay
        // (currently `exit_plan_mode` for the Approve / Keep
        // planning dialog) can reach the bottom-pane handler. The
        // slot is cleared after the turn completes via the
        // `on_turn_completed` hook so a stale sender never leaks
        // into background sub-runs.
        self.executor
            .set_ask_user_request_tx(self.ask_user_request_tx.clone());
        self.executor
            .set_plan_review_request_tx(self.plan_review_request_tx.clone());

        // Plan mode: surface a one-line mode marker. The tool surface stays
        // stable for cache locality; mutating invocations are denied by the
        // permission/tool preflight layer.
        if self.perm_manager.mode() == crate::cli::permission_manager::PermissionMode::Plan {
            state.push_volatile(
                astra_runtime::turn::agentic_loop::host::VolatileKind::PlanModeMarker,
                "[mode=plan] You are in read-only plan mode. The normal tool surface remains visible for cache stability and exploration, but mutating invocations are blocked until the user approves the plan. Use read-only calls, then call `exit_plan_mode(plan=\"<markdown>\")` when ready.",
            );
        }

        // Drain the structured volatile lane before immutable state borrows,
        // but do not inline it into `messages[]`. The server resolves the
        // concrete model row and applies prompt-cache capability metadata before
        // deciding whether this lane is safe to inject.
        // If a skill activation overrode the model, use that; otherwise fall back to host default.
        let effective_model_owned = state
            .skills
            .model_override
            .clone()
            .or_else(|| self.model.map(str::to_owned));
        let effective_model = effective_model_owned.as_deref();
        let effective_offering_id = if state.skills.model_override.is_some() {
            None
        } else {
            self.offering_id.as_deref()
        };
        let runtime_volatile_injections = state.take_volatile_pending();
        let runtime_volatile_texts = self
            .input_runtime_volatile_texts
            .iter()
            .map(|content| content.trim().to_string())
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>();

        let interaction_mode = self.turn_interaction_mode_inherent();
        let persistent_restricted_tools = state.restricted_tools.clone();
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        state
            .restricted_tools
            .extend(interaction_scoped_restrictions.iter().cloned());
        // Request-level allowlists may prune the prompt-visible schema in CLI
        // mode. Skill-level `allowed_tools` must not: those are enforced later
        // in runtime interception so the advertised tool schema remains stable
        // across turns for prompt-cache efficiency.
        let request_scoped_restrictions = request_allowlist_restriction_names(
            &self.all_schemas,
            state.skills.request_constraints.allowed_tools.as_ref(),
        );
        state
            .restricted_tools
            .extend(request_scoped_restrictions.iter().cloned());

        // A textless provider response gets one bounded settlement call. The
        // runtime state is authoritative: remove every advertised schema for
        // this boundary so "produce the final answer" is enforced by the
        // capability surface rather than left as prompt-only guidance.
        if state.hooks.completion_settlement.text_only {
            state
                .restricted_tools
                .extend(tool_names_from_schemas(&self.all_schemas));
        }

        // Plan mode is a permission overlay, not a schema-pruning policy.
        // This returns an empty set by design so plan/default transitions do
        // not churn tool schemas or poison prompt-cache boundaries.
        let plan_scoped_restrictions = plan_mode_restriction_names(
            self.perm_manager.mode() == crate::cli::permission_manager::PermissionMode::Plan,
            &self.all_schemas,
        );
        state
            .restricted_tools
            .extend(plan_scoped_restrictions.iter().cloned());

        // Propagate skill sandbox policy to the tool executor for this turn.
        // The guard restores the previous policy on drop — including on the
        // `?` early-return path below — so a turn that errored out cannot leak
        // a skill-scoped policy into subsequent turns.
        let _sandbox_guard = SandboxPolicyGuard::install(
            &self.executor.sandbox_policy,
            state.skills.sandbox_policy.clone(),
        );
        let send_message_context = state
            .messaging
            .mailbox
            .as_ref()
            .map(
                |mailbox| crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                    agent_id: mailbox.address.agent_id.clone(),
                    run_id: state
                        .current_run_id
                        .clone()
                        .unwrap_or_else(|| mailbox.address.run_id.clone()),
                    router: mailbox.router(),
                },
            )
            .or_else(|| self.root_send_message_context.clone())
            .map(|mut context| {
                if let Some(run_id) = state.current_run_id.clone() {
                    context.run_id = run_id;
                }
                context
            });
        self.executor.set_send_message_context(send_message_context);

        // Inject task board context into the agent's system prompt so it
        // stays aware of what it should be working on this turn.
        // Flows through plan_resume_hint (separate from user-provided
        // append_system_prompt), routed into the standard context pipeline.
        let task_hint = self.executor.build_task_context_hint().await;
        let append_system_prompt = self.append_system_prompt.as_deref();

        macro_rules! fetch_turn_sse {
            () => {
                fetch_chat_turn_sse(ChatTurnSseFetchRequest {
                    api: self.api,
                    token: self.token.as_str(),
                    auth_profile: self.auth_profile,
                    offering_id: effective_offering_id,
                    model: effective_model,
                    context_window_tokens: self.context_window_tokens,
                    effective_input_budget_tokens: state.max_turn_input_tokens,
                    explain: self.explain,
                    render_md: self.render_md,
                    term_width: self.term_width,
                    render_policy: self.render_policy,
                    message: self.message,
                    user_intent: self.user_intent,
                    semantic_query_override: self.semantic_query_override,
                    history: self.history,
                    recent_tools: self.recent_tools,
                    project_root: self.project_root.as_path(),
                    executor: Arc::clone(&self.executor),
                    registry: &self.registry,
                    messages: state.messages.as_slice(),
                    runtime_required_texts: self.input_runtime_required_texts,
                    active_system_skills: self.input_active_system_skills,
                    runtime_volatile_texts: &runtime_volatile_texts,
                    runtime_volatile_injections: &runtime_volatile_injections,
                    ephemeral_prefix: state.skills.listing_message.as_ref(),
                    current_session_id: state.current_session_id.as_deref(),
                    tool_results: state.tool_results.as_slice(),
                    all_schemas: &self.all_schemas,
                    valid_tool_names: &mut self.valid_tool_names,
                    turn_guard: &state.turn_guard,
                    restricted_tools: &mut state.restricted_tools,
                    widen_selection_pending: &mut state.widen_selection_pending,
                    step_recorder: &mut state.step_recorder,
                    file_context: &self.file_context,
                    assembly_start,
                    telem: PrepareTurnTelemetry {
                        first_memoria_ms: &mut state.telemetry.first_memoria_ms,
                        first_selection_report: &mut state.telemetry.first_selection_report,
                        first_budget_pressure: &mut state.telemetry.first_budget_pressure,
                        first_context_assembly_ms: &mut state.telemetry.first_context_assembly_ms,
                        all_selected_skills: &mut state.telemetry.all_selected_skills,
                        trace_collector: state.telemetry.turn_trace_collector.as_ref(),
                    },
                    perm_manager: self.perm_manager,
                    pre_clear_lines: pre_clear,
                    is_plan_subtask: self.is_plan_subtask,
                    plan_subtask_id: self.plan_subtask_id,
                    cancel_token: state.cancellation.token.as_deref(),
                    plan_assemble_line_release: self.plan_assemble_line_release.clone(),
                    stream_event_tx: self.stream_event_tx.clone(),
                    approval_request_tx: self.approval_request_tx.clone(),
                    ask_user_request_tx: self.ask_user_request_tx.clone(),
                    skill_resolver: state.skills.resolver.clone(),
                    skill_effort: state.skills.effort.as_ref().map(|e| e.to_string()),
                    skill_agent_type: state.skills.agent_type.clone(),
                    interaction_mode,
                    turn_policy: &mut state.last_turn_policy,
                    skill_allowed_tools: state
                        .skills
                        .allowed_tools
                        .as_ref()
                        .map(|s| s.iter().cloned().collect::<Vec<_>>()),
                    skill_continuation: state.skill_produced_output,
                    tool_cache: &mut self.tool_cache,
                    previous_confidence_fallback: state
                        .last_confidence_diagnosis
                        .as_ref()
                        .map(|d| d.fallback.clone()),
                    round_index: state.current_round_index,
                    session_turn: state.session_turn,
                    turn_chain_id: state.bridge_turn_chain_id.as_deref(),
                    user_query_event_id: state.bridge_user_query_event_id.as_deref(),
                    observability_hub: state.telemetry.observability_hub.as_ref(),
                    incremental_state: self.incremental_state.clone(),
                    append_system_prompt,
                    plan_resume_hint: task_hint.as_deref(),
                })
                .await
            };
        }

        let mut turn_result = fetch_turn_sse!();
        let stale_expected_turn = turn_result.as_ref().ok().and_then(|result| {
            bridge_session_turn_stale_expected_turn(&result.core, state.session_turn)
        });
        if let Some(expected_turn) = stale_expected_turn {
            let previous_turn = state.session_turn;
            tracing::warn!(
                target: "astra_cli::chat_stream",
                previous_turn,
                expected_turn,
                session_id = state.current_session_id.as_deref().unwrap_or(""),
                "bridge session_turn stale conflict; resynchronizing local turn cursor and retrying once"
            );
            state.session_turn = expected_turn;
            self.chat_turn_index = expected_turn;
            if let Some(ref collector) = state.telemetry.turn_trace_collector {
                collector.set_turn_id(format!("turn-{expected_turn}"));
            }
            turn_result = fetch_turn_sse!();
        }

        // Request overlays must not erase restrictions that were already
        // owned by a capability/permission boundary before this LLM call.
        state.restricted_tools = persistent_restricted_tools;

        // The sandbox policy is restored automatically when `_sandbox_guard`
        // drops at the end of this scope — including the `?` early-return on
        // `turn_result?` below.

        // Sync latest approval overrides into state for checkpoint persistence.
        refresh_root_permission_context(&mut state.permission_context, self.perm_manager).await;
        state.approval_overrides = self.perm_manager.export_session_overrides();

        let turn_result = turn_result?;

        // Write per-round token counts into incremental state immediately
        // so force-exit recovery has accurate prompt/completion/cache tokens
        // even when on_turn_completed never fires.
        if let Some(ref inc) = self.incremental_state {
            inc.set_prompt_tokens(turn_result.core.prompt_tokens);
            inc.set_completion_tokens(turn_result.core.completion_tokens);
            inc.set_cache_read_tokens(turn_result.core.cache_read_tokens);
            inc.set_cache_creation_tokens(turn_result.core.cache_creation_tokens);
        }

        if let Some(refreshed_token) = turn_result.refreshed_token.clone() {
            self.executor.set_cloud_token(refreshed_token.clone());
            self.token = refreshed_token;
        }

        // ─── Injection-freshness observation (post-turn) ────────────────────
        // wip-7: split observation across two input lanes.
        //   1. CLI-owned raw text for channels the CLI authoritatively
        //      knows (lessons from session_lessons_snapshot, self_awareness
        //      from build_self_model_snapshot). These fingerprint with a
        //      full preview so introspect can render the first 80 chars.
        //   2. Bridge-supplied opaque fingerprints for the bridge-internal
        //      channels (memoria_prefetch, tool_round_guidance,
        //      volatile_pending) and the CLI-visible
        //      channels the bridge echoes (recent_arg_hints,
        //      skill_listing) whose source strings the CLI doesn't have
        //      trivial post-turn access to. Wire carries only
        //      hash+bytes+is_empty — no raw text leaves the bridge.
        //
        // If the bridge's `injection_freshness` SSE event was missing
        // (`bridge_injection_fingerprints` is None), the bridge-internal
        // channels are NOT defaulted to empty — they stay Untracked so
        // the freshness report surfaces the broken pipe rather than
        // hiding it behind uniformly-"empty" entries.
        //
        // `lessons_text` is recomputed here (cheap — `session_lessons_snapshot`
        // is an Arc clone) rather than threaded from `prepare_chat_turn_payload`
        // to avoid widening the prepare/fetch signature. The fingerprint uses
        // `LessonKind::as_str()` (stable snake_case DB tag), NOT `Debug`, so
        // enum-variant renames do not flip every channel from Stale→Fresh.
        if let Some(session_lock) = &self.executor.observability_session {
            let lessons_text = self
                .executor
                .session_lessons_snapshot()
                .iter()
                .map(|l| format!("{}:{}:{}", l.kind.as_str(), l.trigger_signal, l.action))
                .collect::<Vec<_>>()
                .join("|");
            // Rebuild the self-awareness section off the same
            // SelfModel snapshot the CLI injects into edge_profile so
            // the fingerprint matches the bytes the model saw.
            let self_awareness_text = self
                .executor
                .build_self_model_snapshot()
                .filter(|m| m.has_meaningful_self_awareness())
                .map(|m| m.to_system_prompt_section())
                .unwrap_or_default();
            let bridge_fps = turn_result.core.bridge_injection_fingerprints.as_ref();
            if let Ok(mut session) = session_lock.write() {
                session.observe_bridge_injections_partial(
                    astra_runtime::observability::BridgeInjectionPreviews {
                        lessons: &lessons_text,
                        self_awareness: &self_awareness_text,
                        ..astra_runtime::observability::BridgeInjectionPreviews::EMPTY
                    },
                    bridge_fps,
                );
            }
        }

        let error_kind = turn_result.core.error_kind;
        Ok(HostTurnResult {
            accum: turn_result.core,
            ttft_ms: turn_result.ttft_ms,
            edge_tool_round: turn_result.edge_tool_round,
            error_kind,
        })
    }

    async fn judge_skill_auto_route(
        &mut self,
        state: &AgenticLoopState,
        ctx: SkillAutoRouteJudgeContext<'_>,
    ) -> Option<SkillAutoRouteDecision> {
        if ctx.query.trim().is_empty() || ctx.visible_skills.is_empty() {
            return None;
        }
        let session_id = state.current_session_id.as_ref()?.clone();
        let service_ctx = cli_skill_auto_route_service_context(ctx);
        let client = CliServerProxySummaryClient {
            api: self.api.clone(),
            token: self.token.clone(),
            base_scope: astra_turn_types::InferenceInvocationScope::Session {
                session_id,
                turn: state.session_turn,
                round: state.current_round_index,
                operation_id: "skill_auto_route".to_string(),
                logical_attempt: 0,
            },
            next_logical_attempt: std::sync::atomic::AtomicU32::new(0),
        };
        let judge = CliSummaryClientSkillAutoRouteJudge {
            client: Box::new(client),
        };
        match astra_services::SkillAutoRouteJudge::judge(&judge, &service_ctx).await {
            Ok(Some(skill_name)) => Some(SkillAutoRouteDecision { skill_name }),
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(
                    target: "astra_cli::skill_auto_route_judge",
                    error = %error,
                    "skill auto-route judge failed; continuing without pre-route"
                );
                None
            }
        }
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        let permission_event =
            astra_turn_core::permission::notice::parse_auto_approved_permission(&line);
        if self.render_policy.suppress_headless() {
            let stream_event = permission_event.map_or_else(
                || crate::cli::chat_stream::StreamEvent::StatusLine(line),
                |(tool, reason)| crate::cli::chat_stream::StreamEvent::PermissionAutoApproved {
                    tool,
                    reason,
                },
            );
            self.try_emit_stream_event(stream_event);
            return;
        }
        let line_ref = line.as_str();
        match style {
            HeadlessStderrStyle::Dim => eprintln!("{}", line_ref.dim()),
            HeadlessStderrStyle::Red => eprintln!("{}", line_ref.red()),
            HeadlessStderrStyle::Green => eprintln!("{}", line_ref.green()),
            HeadlessStderrStyle::Yellow => eprintln!("{}", line_ref.yellow()),
            HeadlessStderrStyle::CyanBold => eprintln!("{}", line_ref.cyan().bold()),
            HeadlessStderrStyle::Magenta => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line_ref.magenta());
            }
            HeadlessStderrStyle::DiffAdd => {
                let body = line_ref.strip_prefix('+').unwrap_or(line_ref);
                eprint!("{}", "│ ".dim());
                eprint!("{}", "+".green().bold());
                eprintln!("{}", body.green());
            }
            HeadlessStderrStyle::DiffRemove => {
                let body = line_ref.strip_prefix('-').unwrap_or(line_ref);
                eprint!("{}", "│ ".dim());
                eprint!("{}", "-".red().bold());
                eprintln!("{}", body.red());
            }
            HeadlessStderrStyle::DiffContext => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line_ref.dim());
            }
            HeadlessStderrStyle::Normal => eprintln!("{}", line_ref),
        }
        self.pending_clear_lines += 1;
        let stream_event = permission_event.map_or_else(
            || crate::cli::chat_stream::StreamEvent::StatusLine(line),
            |(tool, reason)| crate::cli::chat_stream::StreamEvent::PermissionAutoApproved {
                tool,
                reason,
            },
        );
        self.try_emit_stream_event(stream_event);
    }

    fn on_compaction(&mut self, event: CompactionEvent) {
        // Stderr fallback (always visible).
        self.emit_headless_line(HeadlessStderrStyle::Dim, event.summary.clone());
        // Structured event for TUI / stream consumers.
        self.try_emit_stream_event(crate::cli::chat_stream::StreamEvent::Compaction(event));
    }

    fn on_agent_communication(&mut self, event: astra_messaging::AgentCommunicationEvent) {
        self.try_emit_stream_event(crate::cli::chat_stream::StreamEvent::AgentCommunication(
            event,
        ));
    }

    fn on_session_bound(&mut self, session_id: &str) {
        if session_id.trim().is_empty() {
            return;
        }
        self.executor.set_active_session_id(session_id.to_string());
        if let Some(spawner) = self.agent_spawner.as_ref() {
            spawner.bind_session(session_id.to_string());
        }
    }

    fn is_quiet(&self) -> bool {
        self.render_policy.is_silent()
    }

    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        self.turn_interaction_mode_inherent()
    }

    fn plan_mode_active(
        &self,
        _state: &astra_runtime::turn::agentic_loop::host::AgenticLoopState,
    ) -> bool {
        self.perm_manager.mode() == crate::cli::permission_manager::PermissionMode::Plan
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tool_names
    }

    fn deferred_tool_names(&self) -> HashSet<String> {
        self.executor.current_activatable_tool_names_snapshot()
    }

    fn on_user_intent_applied(
        &mut self,
        event: &astra_runtime::turn::run_control::QueuedUserIntent,
    ) {
        if let Some(event) = user_intent_stream_event(event) {
            self.try_emit_stream_event(event);
        }
    }

    fn capabilities(&self) -> astra_turn_core::capability::CapabilitySet {
        self.capabilities.clone()
    }

    fn turn_start_lifecycle_summary(
        &self,
        _state: &astra_runtime::turn::agentic_loop::host::AgenticLoopState,
    ) -> String {
        self.append_system_prompt.clone().unwrap_or_default()
    }

    fn on_introspect_snapshot(
        &mut self,
        snapshot: &astra_turn_core::introspect::IntrospectSnapshot,
    ) {
        self.executor.update_introspect_snapshot(snapshot.clone());
    }

    async fn recover_missing_control_tool_result(
        &mut self,
        parent_run_id: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> ControlToolRecovery {
        if !matches!(tool_name, "agent" | "agent_fanout") {
            return ControlToolRecovery::Unsupported;
        }
        let Some(spawn_context) = self.executor.spawn_context.as_ref() else {
            tracing::warn!(
                target: "astra_cli::agentic_loop_host",
                tool_call_id,
                "agent_fanout recovery skipped: missing spawn context"
            );
            return ControlToolRecovery::Missing;
        };

        let Some(parent_run_id) = parent_run_id else {
            tracing::warn!(
                target: "astra_cli::agentic_loop_host",
                spawn_context_run_id = %spawn_context.run_id,
                tool_call_id,
                "control-tool recovery skipped: missing parent run identity"
            );
            return ControlToolRecovery::Missing;
        };
        if parent_run_id != spawn_context.run_id {
            tracing::warn!(
                target: "astra_cli::agentic_loop_host",
                parent_run_id,
                spawn_context_run_id = %spawn_context.run_id,
                tool_call_id,
                "control-tool recovery skipped: parent run does not own the active spawn context"
            );
            return ControlToolRecovery::Missing;
        }

        if tool_name == "agent" {
            let started_at = std::time::Instant::now();
            let mut execution_args = args.clone();
            if let Some(object) = execution_args.as_object_mut() {
                object.insert(
                    "_tool_call_id".to_string(),
                    Value::String(tool_call_id.to_string()),
                );
            }
            let action = agent_control_action(args).map(str::to_string);
            let label = action
                .as_deref()
                .map(|action| agent_control_label(args, format!("Agent {action}")));
            if let (Some(action), Some(label)) = (action.as_deref(), label.as_deref()) {
                self.try_emit_stream_event(
                    crate::cli::chat_stream::StreamEvent::AgentControlStarted {
                        action: action.to_string(),
                        label: label.to_string(),
                        tool_use_id: tool_call_id.to_string(),
                        agent_id: agent_id_from_args(args),
                        fanout_slot: None,
                        fanout_title: None,
                    },
                );
                self.try_emit_stream_event(crate::cli::chat_stream::StreamEvent::ToolStarted {
                    name: tool_name.to_string(),
                    description: label.to_string(),
                    tool_use_id: tool_call_id.to_string(),
                    parent_tool_use_id: None,
                });
            }
            let outcome = self
                .executor
                .execute_with_metadata(tool_name, &execution_args)
                .await;
            let duration_ms = started_at.elapsed().as_millis() as u64;
            let status = if outcome.is_error {
                "failed"
            } else {
                cloud_tool_result_status_label(&outcome.output)
            }
            .to_string();
            let event_output = tool_output_event_text(tool_name, &outcome.output);
            if let (Some(action), Some(label)) = (action.as_deref(), label.as_deref()) {
                self.try_emit_stream_event(
                    crate::cli::chat_stream::StreamEvent::AgentControlCompleted {
                        action: action.to_string(),
                        label: label.to_string(),
                        status: status.clone(),
                        duration_ms,
                        output: Some(event_output.clone()),
                        tool_use_id: tool_call_id.to_string(),
                        agent_id: agent_id_from_output(&outcome.output)
                            .or_else(|| agent_id_from_args(args)),
                    },
                );
                self.try_emit_stream_event(crate::cli::chat_stream::StreamEvent::ToolCompleted {
                    name: tool_name.to_string(),
                    description: label.to_string(),
                    status: status.clone(),
                    duration_ms,
                    output_summary: None,
                    output: Some(event_output),
                    tool_use_id: tool_call_id.to_string(),
                    parent_tool_use_id: None,
                });
            }
            return ControlToolRecovery::Recovered(EdgeToolExecResult {
                request_id: tool_call_id.to_string(),
                tool: tool_name.to_string(),
                args: args.clone(),
                output: outcome.output,
                tool_result_fields: None,
                status,
                duration_ms,
            });
        }

        let output = match tokio::time::timeout(
            AGENT_FANOUT_RECOVERY_TIMEOUT,
            astra_runtime::orchestration::recover_agent_fanout_tool_result(
                args,
                Some(tool_call_id),
                Some(spawn_context),
            ),
        )
        .await
        {
            Ok(output) => output,
            Err(_) => render_control_tool_recovery_error(
                "Cannot recover missing agent_fanout edge result: recovery timed out before the host could render the registered fanout group.",
            ),
        };
        let status = cloud_tool_result_status_label(&output).to_string();
        ControlToolRecovery::Recovered(EdgeToolExecResult {
            request_id: tool_call_id.to_string(),
            tool: tool_name.to_string(),
            args: args.clone(),
            output,
            tool_result_fields: None,
            status,
            duration_ms: 0,
        })
    }

    async fn cancel_child_agents(&mut self, agent_ids: &[String], reason: &str) -> Vec<String> {
        let Some(spawn_context) = self.executor.spawn_context.as_ref() else {
            return Vec::new();
        };

        let mut cancelled = Vec::new();
        for agent_id in agent_ids {
            if spawn_context.spawner.cancel_agent(agent_id, reason).await {
                cancelled.push(agent_id.clone());
            }
        }
        cancelled
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        crate::cli::tool_surface_injection::install_injected_tool_schema(
            self.executor.as_ref(),
            schema,
            &mut self.all_schemas,
            &mut self.valid_tool_names,
            Some(&mut self.registry),
        );
    }

    fn render_final_text(&mut self, text: &str) {
        use std::io::Write;
        if self.render_policy.suppress_final_text() {
            return;
        }
        if text.is_empty() {
            return;
        }
        if self.render_md {
            let mut md = crate::cli::stream::streaming_md::StreamingMarkdown::new(self.term_width);
            md.push(text);
            md.finish();
        } else {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            let _ = std::io::stdout().flush();
        }
    }

    async fn on_final_output_ready(&mut self, _state: &AgenticLoopState) {
        emit_final_output_ready(
            self.stream_event_tx.as_ref(),
            &mut self.pending_ordered_stream_events,
        )
        .await;
    }

    fn on_turn_completed(
        &mut self,
        state: &astra_runtime::turn::agentic_loop::host::AgenticLoopState,
    ) {
        // Drop the per-turn ask_user channel so a stale sender from
        // this turn doesn't leak into background sub-runs that share
        // the same `Arc<ToolExecutor>` (the channel is reinstalled at
        // the start of every turn).
        self.executor.set_ask_user_request_tx(None);
        self.executor.set_plan_review_request_tx(None);

        // Sync incremental state for interruption recovery.
        // Token counts are updated per-round in execute_turn (so
        // force-exit captures accurate cumulative totals).  Here we
        // sync non-token fields: session_id, run_id, and tool records.
        if let Some(ref inc) = self.incremental_state {
            if let Some(ref sid) = state.current_session_id {
                if !sid.is_empty() {
                    inc.set_session_id(sid.clone());
                }
            }
            if let Some(ref rid) = state.current_run_id {
                inc.set_run_id(rid.clone());
            }
            let tool_records = state
                .stall
                .tool_call_records
                .iter()
                .filter(|record| {
                    !record.is_synthetic_placeholder() && !record.was_blocked_by_policy()
                })
                .cloned()
                .collect::<Vec<_>>();
            let tools_used = tool_records.iter().map(|record| record.name.clone()).fold(
                Vec::new(),
                |mut acc, name| {
                    if !acc.iter().any(|existing| existing == &name) {
                        acc.push(name);
                    }
                    acc
                },
            );
            inc.replace_tool_records(tool_records);
            inc.replace_tools_used(tools_used);
        }

        // Bug B step 3: capture the parent turn's cacheable prefix
        // so subsequent agent-spawn / delegate calls can inherit it
        // for prompt-cache reuse. No-op unless:
        //   - the `prefix_store` Arc was plumbed in by the host
        //   - ingest populated the expected state fields
        let Some(store) = self.prefix_store.as_ref() else {
            return;
        };
        let parent_run_id = match state.current_run_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        let model_selector = state
            .skills
            .model_override
            .as_deref()
            .or(self.model)
            .unwrap_or("");
        let model_id = model_selector.to_string();
        let provider = astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(&model_id);
        let raw_provider = provider.raw_provider_name().to_owned();
        let capture_thinking =
            astra_turn_core::thinking_config::resolve_model_thinking(model_selector).1;
        // Canonical prefix bytes: JSON-serialize the messages as-is.
        // This is the format `fork_reconstruct::reconstruct_messages`
        // expects on the consuming end. System prompts and tool
        // schemas are captured separately; for step 3 we ship a
        // minimal-but-correct subset — the messages array is what
        // downstream prepending into child state cares about.
        let Ok(canonical_prefix_bytes) = serde_json::to_vec(&state.messages) else {
            return;
        };
        let captured_at_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // G1 payload fill: `all_schemas` is exactly the tool list the
        // CLI advertised to the LLM this turn, so it's the honest
        // source for `tool_schemas`. `system_blocks` stays empty here
        // because the CLI doesn't assemble the final system prompt
        // locally — the server-side bridge does that. A future PR on
        // the server host (G2) will populate system_blocks where the
        // real bytes exist.
        let tool_schemas =
            astra_turn_core::fork_prefix::build_tool_schema_entries(&self.all_schemas);
        let req = astra_turn_core::fork_capture::CaptureRequest {
            parent_run_id,
            parent_turn_seq: self.chat_turn_index,
            provider,
            model_id,
            thinking: astra_turn_core::thinking_config::fork_capture_thinking_slice(
                &capture_thinking,
                &raw_provider,
                model_selector,
            ),
            system_blocks: vec![],
            tool_schemas,
            beta_headers: vec![],
            canonical_prefix_bytes,
            cache_mode: astra_turn_core::fork_prefix::CacheMode::Write,
            captured_at_secs,
            microcompact_fired_in_turn: false,
        };
        let _ = astra_turn_core::fork_capture::capture_parent_prefix(req, store.as_ref());
    }
}

/// Plan mode is a permission overlay, not a schema-pruning pass.
///
/// Return no `restricted_tools` so the model sees the same capability surface
/// before and during planning. Mutating invocations are blocked later by the
/// args-aware plan-mode policy, which avoids prompt-cache churn and preserves
/// read-only shell exploration.
fn plan_mode_restriction_names(
    _plan_active: bool,
    _schemas: &[serde_json::Value],
) -> HashSet<String> {
    HashSet::new()
}

fn request_allowlist_restriction_names(
    schemas: &[serde_json::Value],
    request_allowed: Option<&HashSet<String>>,
) -> HashSet<String> {
    let effective_allowed =
        astra_turn_core::tool_allowlist::compute_effective_allowlist(request_allowed, None);
    let Some(allowed) = effective_allowed else {
        return HashSet::new();
    };

    schemas
        .iter()
        .filter_map(|schema| {
            schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str)
        })
        .filter(|name| !allowed.contains(*name))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CliSummaryClientSkillAutoRouteJudge, derive_turn_interaction_mode, emit_final_output_ready,
        permission_mode_change_audit_event, plan_mode_restriction_names,
        request_allowlist_restriction_names, user_intent_stream_event,
    };
    use crate::cli::permission_manager::PermissionMode;
    use astra_runtime::turn::agentic_loop::host::TurnInteractionMode;
    use astra_services::session_journal::JournalEventType;
    use serde_json::json;
    use std::collections::HashSet;

    #[tokio::test]
    async fn final_output_ready_reaches_the_typed_stream_lane() {
        let (tx, mut rx) = crate::cli::chat_stream::stream_event_channel();

        emit_final_output_ready(Some(&tx), &mut std::collections::VecDeque::new()).await;

        assert!(matches!(
            rx.recv().await,
            Some(crate::cli::chat_stream::StreamEvent::AssistantOutputSettled)
        ));
    }

    #[tokio::test]
    async fn terminal_drain_preserves_ordered_control_events_after_backpressure() {
        use crate::cli::chat_stream::StreamEvent;

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(StreamEvent::StatusLine("already buffered".into()))
            .await
            .unwrap();
        let mut pending = std::collections::VecDeque::from([
            StreamEvent::ToolStarted {
                name: "agent_fanout".into(),
                description: "Review".into(),
                tool_use_id: "call-1".into(),
                parent_tool_use_id: None,
            },
            StreamEvent::ToolCompleted {
                name: "agent_fanout".into(),
                description: "Review".into(),
                status: "completed".into(),
                duration_ms: 10,
                output_summary: None,
                output: None,
                tool_use_id: "call-1".into(),
                parent_tool_use_id: None,
            },
        ]);
        let consumer = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(event) = rx.recv().await {
                let settled = matches!(event, StreamEvent::AssistantOutputSettled);
                received.push(event);
                if settled {
                    break;
                }
            }
            received
        });

        emit_final_output_ready(Some(&tx), &mut pending).await;
        let received = consumer.await.unwrap();

        assert!(matches!(received[0], StreamEvent::StatusLine(_)));
        assert!(matches!(received[1], StreamEvent::ToolStarted { .. }));
        assert!(matches!(received[2], StreamEvent::ToolCompleted { .. }));
        assert!(matches!(received[3], StreamEvent::AssistantOutputSettled));
    }

    struct ScriptedSummaryClient {
        text: String,
    }

    #[async_trait::async_trait]
    impl astra_turn_core::cloud_summary::SummaryLlmClient for ScriptedSummaryClient {
        async fn summarize(
            &self,
            _purpose: astra_turn_types::InferencePurpose,
            _messages: &[serde_json::Value],
        ) -> Result<astra_turn_core::cloud_summary::SummaryResponse, String> {
            Ok(astra_turn_core::cloud_summary::SummaryResponse {
                text: self.text.clone(),
                is_ptl_error: false,
            })
        }
    }

    #[tokio::test]
    async fn cli_skill_auto_route_judge_uses_llm_response_parser() {
        let judge = CliSummaryClientSkillAutoRouteJudge {
            client: Box::new(ScriptedSummaryClient {
                text: r#"{"skill_name":"review-changes"}"#.to_string(),
            }),
        };
        let ctx = astra_services::SkillAutoRouteJudgeContext {
            query: "review local changes".to_string(),
            visible_skills: vec![astra_services::SkillAutoRouteCandidate {
                name: "review-changes".to_string(),
                description: "Review changed files".to_string(),
                when_to_use: Some("Use when the user asks for code review".to_string()),
                aliases: vec!["review".to_string()],
            }],
        };

        let selected = astra_services::SkillAutoRouteJudge::judge(&judge, &ctx)
            .await
            .expect("judge response should parse");

        assert_eq!(selected, Some("review-changes".to_string()));
    }

    #[test]
    fn permission_mode_change_audit_event_carries_source_and_modes() {
        let event = permission_mode_change_audit_event(
            Some("sess-audit"),
            7,
            PermissionMode::Plan,
            PermissionMode::Auto,
            "plan_approval_overlay",
        );

        assert_eq!(event.event_type, JournalEventType::PermissionAudit);
        assert_eq!(event.session_id.as_deref(), Some("sess-audit"));
        assert_eq!(event.turn, Some(7));
        let metadata = event.metadata.expect("permission audit metadata");
        assert_eq!(metadata["kind"], "permission_mode_changed");
        assert_eq!(metadata["from_mode"], "plan");
        assert_eq!(metadata["to_mode"], "auto");
        assert_eq!(metadata["source"], "plan_approval_overlay");
        assert_eq!(metadata["changed"], true);
    }

    #[test]
    fn derive_turn_interaction_mode_maps_permission_mode_with_native_prompt_sink() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, true, false, true),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(
                PermissionMode::AcceptEdits,
                false,
                false,
                true,
                false,
                true,
            ),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, false, true),
            TurnInteractionMode::Auto
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Bypass, false, false, false, false, true),
            TurnInteractionMode::Auto
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Deny, false, false, false, false, true),
            TurnInteractionMode::Deny
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_for_subtasks_and_silent_turns() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, true, false, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn user_intent_stream_event_preserves_identity_and_content() {
        let event = user_intent_stream_event(&astra_runtime::turn::run_control::QueuedUserIntent {
            intent_id: "input-7".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::AcceptedLocal,
            event_index: 7,
            input: json!({"content": "先停啊！"}),
        })
        .expect("user intent feedback should be emitted");
        assert!(matches!(
            event,
            crate::cli::chat_stream::StreamEvent::UserIntentApplied {
                intent_id,
                event_index: 7,
                content,
                ..
            } if intent_id == "input-7" && content == "先停啊！"
        ));
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_without_tty_or_native_prompt_sink() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, true, true, false, true),
            TurnInteractionMode::Prompt
        );
    }

    // ── Auto mode preservation under non-interactive contexts ─────────
    //
    // Auto is the permission/presentation mode selected for the root turn.
    // Piped stdin, silent rendering, and approval-channel availability only
    // affect whether Prompt mode can pause; they must not silently rewrite
    // Auto semantics. Policy feedback remains model-visible in every mode.

    #[test]
    fn derive_turn_interaction_mode_preserves_auto_without_tty() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, false, false),
            TurnInteractionMode::Auto,
            "Auto must NOT be demoted to NonInteractive just because stdin is piped — \
             user's opt-in to uninterrupted execution still applies"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_preserves_auto_with_silent_render() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, true, true),
            TurnInteractionMode::Auto,
            "silent render (e.g. --quiet or harness) must not override Auto intent"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_preserves_auto_with_approval_channel() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, true, false, false, true),
            TurnInteractionMode::Auto,
            "approval-tx (e.g. web-approval flow) must not demote Auto interaction mode"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_demotes_auto_only_for_plan_subtask() {
        // Plan subtasks are structurally non-interactive: the subtask
        // agent has no user-facing session, and injected nudges land
        // in a throwaway context. Auto→NonInteractive here is OK.
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, true, false, false, false, true),
            TurnInteractionMode::NonInteractive,
            "plan subtasks have no user-facing mode distinction — NonInteractive"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_demotes_deny_like_prompt() {
        // Deny mode under non-interactive also falls back to
        // NonInteractive (no opportunity to refuse anything
        // interactively — and Deny's behaviour is already deterministic
        // denial, same as NonInteractive's restrictive default).
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Deny, false, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn derive_turn_interaction_mode_uses_deny_for_interactive_plan() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Plan, false, false, false, false, true),
            TurnInteractionMode::Deny
        );
    }

    #[test]
    fn derive_turn_interaction_mode_uses_noninteractive_for_structural_plan() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Plan, false, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    // ── Plan-mode restriction lifecycle ─────────────────────────
    //
    // Regression for session 19298aea-06de-49bf-a1b5-2873c7e87b9e:
    // the model entered plan mode, called `exit_plan_mode`, the
    // overlay flipped perm_manager to Auto on the next turn — but
    // `restricted_tools` had been `extend()`-ed with the plan-mode
    // mutating tools and never cleared. The whole rest of the
    // session, write_file / bash / str_replace stayed restricted,
    // and the model was reduced to telling the user "please run
    // these commands yourself".
    //
    // The contract the lifecycle helpers below pin: plan-mode
    // restrictions are *turn-scoped* — added when the turn opens
    // with plan-mode active, removed unconditionally when the turn
    // ends. Same shape as `interaction_scoped_tool_restrictions`.

    fn schema(name: &str) -> serde_json::Value {
        serde_json::json!({"type": "function", "function": {"name": name}})
    }

    #[test]
    fn plan_mode_restriction_names_do_not_hide_schema_in_plan_mode() {
        let schemas = vec![
            schema("read_file"),
            schema("grep"),
            schema("write_file"),
            schema("str_replace"),
            schema("bash"),
            schema("exit_plan_mode"),
            schema("enter_plan_mode"),
        ];
        let plan_off = plan_mode_restriction_names(false, &schemas);
        assert!(
            plan_off.is_empty(),
            "plan-mode restrictions must be empty when plan mode is off"
        );

        let plan_on = plan_mode_restriction_names(true, &schemas);
        assert!(
            plan_on.is_empty(),
            "plan mode must not hide tool schemas; args-aware preflight blocks mutating calls"
        );
    }

    #[test]
    fn request_allowlist_restriction_names_hides_non_request_tools() {
        let schemas = vec![
            schema("git"),
            schema("read_file"),
            schema("str_replace"),
            schema("write_file"),
        ];
        let request_allowed = HashSet::from(["git".to_string(), "read_file".to_string()]);

        let restricted = request_allowlist_restriction_names(&schemas, Some(&request_allowed));

        assert!(!restricted.contains("git"));
        assert!(!restricted.contains("read_file"));
        assert!(restricted.contains("str_replace"));
        assert!(restricted.contains("write_file"));
    }

    #[test]
    fn request_allowlist_restriction_names_normalizes_names() {
        let schemas = vec![schema("git"), schema("read_file"), schema("str_replace")];
        let request_allowed = HashSet::from([" Git ".to_string(), "READ_FILE".to_string()]);

        let restricted = request_allowlist_restriction_names(&schemas, Some(&request_allowed));

        assert!(!restricted.contains("git"));
        assert!(!restricted.contains("read_file"));
        assert!(restricted.contains("str_replace"));
    }

    #[test]
    fn plan_mode_restrictions_do_not_pollute_later_turns() {
        let schemas = vec![
            schema("read_file"),
            schema("write_file"),
            schema("bash"),
            schema("exit_plan_mode"),
        ];
        let mut restricted: HashSet<String> = HashSet::new();

        // Turn N: plan mode is active but does not mutate hard restrictions.
        let plan_set = plan_mode_restriction_names(true, &schemas);
        restricted.extend(plan_set.iter().cloned());
        assert!(restricted.is_empty());

        // Turn N ends — host removes the names it added.
        for name in &plan_set {
            restricted.remove(name);
        }
        assert!(
            restricted.is_empty(),
            "plan-mode schema policy must not leave stale hard restrictions (regression: session 19298aea)"
        );

        // Turn N+1: plan mode off after exit_plan_mode → no restrictions.
        let plan_off = plan_mode_restriction_names(false, &schemas);
        restricted.extend(plan_off.iter().cloned());
        assert!(
            !restricted.contains("write_file"),
            "next turn after exit_plan_mode must let write_file through"
        );
        assert!(
            !restricted.contains("bash"),
            "next turn after exit_plan_mode must let bash through"
        );
    }

    #[test]
    fn plan_mode_restrictions_do_not_clobber_caller_existing_entries() {
        // The host shares `restricted_tools` between several lifecycle
        // owners (interaction-scoped, plan-scoped, stall-scoped, etc.).
        // Removing plan-scoped names must leave entries that other
        // owners added in place.
        let schemas = vec![schema("write_file"), schema("bash"), schema("read_file")];
        let mut restricted: HashSet<String> = HashSet::new();
        // Pretend an unrelated subsystem already restricted `ask_user`.
        restricted.insert("ask_user".to_string());

        let plan_set = plan_mode_restriction_names(true, &schemas);
        restricted.extend(plan_set.iter().cloned());
        for name in &plan_set {
            restricted.remove(name);
        }

        assert!(
            restricted.contains("ask_user"),
            "plan-mode cleanup must not delete entries it never owned"
        );
        assert_eq!(restricted.len(), 1);
    }

    #[test]
    fn on_compaction_forwards_via_channel() {
        // Verify that compaction events forwarded through the stream channel
        // arrive with correct kind and summary.
        use crate::cli::chat_stream::StreamEvent;
        use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind};
        let (tx, mut rx) = crate::cli::chat_stream::stream_event_channel();
        let event = CompactionEvent {
            kind: CompactionKind::ReactiveBudget,
            pressure: 0.85,
            tokens_freed: 12000,
            tokens_before: 48000,
            tokens_after: 36000,
            max_tokens: 64000,
            messages_removed: 8,
            messages_after: 42,
            layer_descriptions: vec!["old_turns: ~8000".into(), "tool_outputs: ~4000".into()],
            summary: "reactive budget compaction".into(),
        };

        // Same pattern used by CliAgenticLoopHost::on_compaction:
        tx.try_send(StreamEvent::Compaction(event)).unwrap();

        let received = rx.try_recv().expect("must receive compaction event");
        match received {
            StreamEvent::Compaction(e) => {
                assert_eq!(e.kind, CompactionKind::ReactiveBudget);
                assert_eq!(e.pressure, 0.85);
                assert_eq!(e.tokens_freed, 12000);
                assert_eq!(e.summary, "reactive budget compaction");
            }
            other => panic!("expected Compaction event, got {other:?}"),
        }
    }
}
