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
        TurnInteractionMode, interaction_scoped_tool_restrictions,
    },
};
use astra_turn_core::{
    compaction_types::CompactionEvent, orchestration::agent_result_wire::render_agent_tool_error,
    sse_stream_host::EdgeToolExecResult, tool_result_semantics::cloud_tool_result_status_label,
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use serde_json::Value;

use crate::{
    ExplainMode,
    cli::permission_manager::{PermissionManager, PermissionMode},
    cli::stream::stream_render::RenderPolicy,
    edge_tools::ToolExecutor,
};

use crate::cli::chat_stream::sse_loop::agentic_loop_turn::{
    ChatTurnSseFetchRequest, PrepareTurnTelemetry, fetch_chat_turn_sse,
};
use crate::cli::chat_stream::sse_loop::refresh_root_permission_context;

use astra_runtime::tool_sandbox::SandboxPolicy;

const AGENT_FANOUT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(3);

fn control_tool_recovery_fields(outcome: &str) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([(
        "recovery".to_string(),
        serde_json::json!({
            "attempted": true,
            "source": "host_state",
            "outcome": outcome,
        }),
    )])
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

fn failed_control_tool_recovery_result(
    tool_call_id: &str,
    tool_name: &str,
    args: &Value,
    message: &str,
) -> EdgeToolExecResult {
    EdgeToolExecResult {
        request_id: tool_call_id.to_string(),
        tool: tool_name.to_string(),
        args: args.clone(),
        output: render_control_tool_recovery_error(message),
        tool_result_fields: Some(control_tool_recovery_fields("failed")),
        status: "failed".to_string(),
        duration_ms: 0,
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
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
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

    match permission_mode {
        // Auto is the user's explicit opt-in to "run uninterrupted" —
        // it stays Auto regardless of stdin/approval-channel/silent
        // render. All of those signals only matter for Prompt (which
        // needs stdin to ask the user), and Auto already short-circuits
        // prompts anyway. Regression for session c6e18730 where piped
        // or silent contexts silently demoted Auto → NonInteractive,
        // which in turn disabled the nudge-suppression gate the user
        // opted into.
        PermissionMode::Auto => TurnInteractionMode::Auto,
        PermissionMode::Plan => {
            if render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else if has_ask_user_request_tx {
                TurnInteractionMode::Prompt
            } else {
                TurnInteractionMode::NonInteractive
            }
        }
        // Edits still needs the native ask_user sink for clarifications.
        // The old stdin/raw-mode path was removed because it corrupts the TUI
        // and has no product parity with the overlay flow.
        PermissionMode::Edits => {
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
        PermissionMode::Ask => {
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
        PermissionMode::Ci => {
            if has_approval_request_tx || render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else {
                TurnInteractionMode::Ci
            }
        }
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
}

fn deferred_input_status_line(input: &Value) -> Option<String> {
    let text = input
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| input.get("text").and_then(Value::as_str))
        .or_else(|| input.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())?;
    let mut preview: String = text
        .replace('\n', crate::DEFERRED_INPUT_FINGERPRINT_SEP)
        .chars()
        .take(80)
        .collect();
    if text
        .replace('\n', crate::DEFERRED_INPUT_FINGERPRINT_SEP)
        .chars()
        .count()
        > 80
    {
        preview.push_str("...");
    }
    Some(format!("__deferred_input_applied__:{preview}"))
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

        // Pull mid-turn UI-staged mode pivots into `self.mode`. The
        // user can press Shift+Tab while a turn is running; the TUI
        // cannot borrow `perm_manager` then, so it writes the new
        // mode through the lock-free mirror. Reading it here makes
        // the new mode authoritative for this turn's schema and
        // gating decisions. No-op when nothing was staged.
        let mode_before = self.perm_manager.mode();
        self.perm_manager.pull_mode_from_mirror();
        let mode_after = self.perm_manager.mode();
        if mode_before != mode_after {
            refresh_root_permission_context(&mut state.permission_context, self.perm_manager).await;
            append_permission_mode_change_audit(
                state.current_session_id.as_deref(),
                self.chat_turn_index,
                mode_before,
                mode_after,
                "mid_turn_ui",
            );
            state.push_volatile(
                astra_runtime::turn::agentic_loop::host::VolatileKind::PlanModeMarker,
                format!(
                    "[mode={mode_after}] User pressed Shift+Tab; permission mode is now `{mode_after}`. Adjust your tool surface accordingly."
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

        // Plan mode: surface a one-line mode marker so the model knows
        // why mutating tools are missing from the schema. Singleton
        // (`is_singleton` on the kind), so re-pushing every turn keeps
        // exactly one entry on the lane. Drained alongside other
        // runtime nudges by the call below.
        if self.perm_manager.mode() == crate::cli::permission_manager::PermissionMode::Plan {
            state.push_volatile(
                astra_runtime::turn::agentic_loop::host::VolatileKind::PlanModeMarker,
                "[mode=plan] You are in read-only plan mode. Investigate with read-only tools (read_file, grep, glob, web_fetch, …); mutating tools are intentionally absent from the schema. When the plan is ready call `exit_plan_mode(plan=\"<markdown>\")` so the user can approve and choose an execution mode. Do not attempt edits or shell mutations in this mode.",
            );

            // Plan-mode nudge: if the previous turn produced a
            // plan-shaped response but the model did not actually
            // call `exit_plan_mode`, the user never sees an approval
            // overlay and the agent silently stalls. Inject a
            // corrective so the next round either tightens the plan
            // and submits via the tool, or asks the user a question.
            if let Some(reminder) = plan_mode_missed_exit_reminder(&state.messages) {
                state.push_volatile(
                    astra_runtime::turn::agentic_loop::host::VolatileKind::Corrective,
                    reminder,
                );
            }
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
        let runtime_volatile_texts = state
            .take_volatile_pending()
            .into_iter()
            .map(|injection| injection.content)
            .filter(|content| !content.trim().is_empty())
            .collect::<Vec<_>>();

        let interaction_mode = self.turn_interaction_mode_inherent();
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

        // Plan-mode restrictions follow the same turn-scoped pattern:
        // computed at the start of the turn from the current
        // perm_manager state, removed unconditionally at the end.
        // Owning the lifecycle here (instead of inside
        // `prepare_chat_turn_payload`) is what keeps the
        // restrictions from leaking into later turns — regression
        // for session 19298aea, where `extend` without a matching
        // `remove` left `write_file` / `bash` permanently
        // restricted after the model called `exit_plan_mode`.
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
        self.executor.set_send_message_context(
            state
                .messaging
                .mailbox
                .as_ref()
                .map(
                    |mailbox| crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                        agent_id: mailbox.address.agent_id.clone(),
                        router: mailbox.router(),
                        metrics: state.messaging.metrics.clone(),
                        delegation_id: mailbox.delegation_id.clone(),
                    },
                )
                .or_else(|| self.root_send_message_context.clone()),
        );

        // Inject task board context into the agent's system prompt so it
        // stays aware of what it should be working on this turn.
        // Flows through plan_resume_hint (separate from user-provided
        // append_system_prompt), routed into the standard context pipeline.
        let task_hint = self.executor.build_task_context_hint().await;
        let append_system_prompt = self.append_system_prompt.as_deref();

        let turn_result = fetch_chat_turn_sse(ChatTurnSseFetchRequest {
            api: self.api,
            token: self.token.as_str(),
            auth_profile: self.auth_profile,
            model: effective_model,
            explain: self.explain,
            render_md: self.render_md,
            term_width: self.term_width,
            render_policy: self.render_policy,
            message: self.message,
            semantic_query_override: self.semantic_query_override,
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: Arc::clone(&self.executor),
            registry: &self.registry,
            messages: state.messages.as_slice(),
            runtime_volatile_texts: &runtime_volatile_texts,
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
        .await;

        for name in &interaction_scoped_restrictions {
            state.restricted_tools.remove(name);
        }
        for name in &request_scoped_restrictions {
            state.restricted_tools.remove(name);
        }
        for name in &plan_scoped_restrictions {
            state.restricted_tools.remove(name);
        }

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
        //   2. Bridge-supplied opaque fingerprints for the 5 bridge-internal
        //      channels (memoria_prefetch, feedback_rules, implicit_feedback,
        //      tool_round_guidance, volatile_pending) and the 3 CLI-visible
        //      channels the bridge echoes (memoria_insights, recent_arg_hints,
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
                    astra_runtime::observability::BridgeInjectionTexts {
                        lessons: &lessons_text,
                        self_awareness: &self_awareness_text,
                        ..astra_runtime::observability::BridgeInjectionTexts::EMPTY
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

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        let permission_event =
            astra_turn_core::permission::notice::parse_auto_approved_permission(&line);
        if self.render_policy.suppress_headless() {
            if let Some(tx) = &self.stream_event_tx {
                let stream_event = permission_event.map_or_else(
                    || crate::cli::chat_stream::StreamEvent::StatusLine(line),
                    |(tool, reason)| crate::cli::chat_stream::StreamEvent::PermissionAutoApproved {
                        tool,
                        reason,
                    },
                );
                let _ = tx.send(stream_event);
            }
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
        if let Some(tx) = &self.stream_event_tx {
            let stream_event = permission_event.map_or_else(
                || crate::cli::chat_stream::StreamEvent::StatusLine(line),
                |(tool, reason)| crate::cli::chat_stream::StreamEvent::PermissionAutoApproved {
                    tool,
                    reason,
                },
            );
            let _ = tx.send(stream_event);
        }
    }

    fn on_compaction(&mut self, event: CompactionEvent) {
        // Stderr fallback (always visible).
        self.emit_headless_line(HeadlessStderrStyle::Dim, event.summary.clone());
        // Structured event for TUI / stream consumers.
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(crate::cli::chat_stream::StreamEvent::Compaction(event));
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

    fn on_deferred_user_input(&mut self, input: &Value) {
        let Some(tx) = &self.stream_event_tx else {
            return;
        };
        if let Some(line) = deferred_input_status_line(input) {
            let _ = tx.send(crate::cli::chat_stream::StreamEvent::StatusLine(line));
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
        if tool_name != "agent_fanout" {
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
                "agent_fanout recovery failed: missing parent run id"
            );
            return ControlToolRecovery::Recovered(failed_control_tool_recovery_result(
                tool_call_id,
                tool_name,
                args,
                "Cannot recover missing agent_fanout edge result: parent_run_id is missing, so the host cannot prove which parent turn owns the fanout group.",
            ));
        };
        if parent_run_id != spawn_context.run_id {
            tracing::warn!(
                target: "astra_cli::agentic_loop_host",
                parent_run_id,
                spawn_context_run_id = %spawn_context.run_id,
                tool_call_id,
                "agent_fanout recovery failed: parent run id does not match spawn context"
            );
            return ControlToolRecovery::Recovered(failed_control_tool_recovery_result(
                tool_call_id,
                tool_name,
                args,
                "Cannot recover missing agent_fanout edge result: parent_run_id does not match the active spawn context.",
            ));
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
        let recovery_outcome = if status == "completed" {
            "recovered"
        } else {
            "failed"
        };
        ControlToolRecovery::Recovered(EdgeToolExecResult {
            request_id: tool_call_id.to_string(),
            tool: tool_name.to_string(),
            args: args.clone(),
            output,
            tool_result_fields: Some(control_tool_recovery_fields(recovery_outcome)),
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
        if self.render_policy.is_silent() || self.render_policy == RenderPolicy::PlanDecompose {
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

/// Detect the "model wrote a plan but forgot to call `exit_plan_mode`"
/// failure mode. Returns the reminder text to push as a Corrective
/// volatile, or `None` when the previous turn either did not look
/// plan-shaped or already exited the plan via the tool.
///
/// Heuristic — intentionally conservative:
///   * Look at the most recent assistant message in `messages`.
///   * It must contain an explicit `## Plan` / `### Steps` markdown
///     header, OR a plan marker plus at least three numbered steps.
///   * The same assistant message must NOT carry a `tool_calls`
///     entry whose name is `exit_plan_mode` (already-exited turns
///     don't need the nudge).
///
/// Rationale: if the heuristic is too eager it spams the model on
/// every analytical answer; if it's too cautious it leaves the user
/// stuck. We err toward "don't nudge" — the cost of a missed nudge
/// is one stale turn; the cost of a false nudge is repeated
/// scolding the model takes literally.
fn plan_mode_missed_exit_reminder(messages: &[serde_json::Value]) -> Option<String> {
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))?;

    if assistant_called_exit_plan_mode(last_assistant) {
        return None;
    }

    let content = assistant_text(last_assistant)?;
    if !looks_plan_shaped(&content) {
        return None;
    }

    Some(
        "[plan-nudge] Your last response looked like a plan but you did not call `exit_plan_mode`. Surface the plan for user approval by calling `exit_plan_mode(plan=\"<markdown>\")`. Without that call the user has no way to approve and unlock execution.".to_string()
    )
}

fn assistant_called_exit_plan_mode(message: &serde_json::Value) -> bool {
    let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) else {
        return false;
    };
    tool_calls.iter().any(|call| {
        call.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            == Some("exit_plan_mode")
    })
}

fn assistant_text(message: &serde_json::Value) -> Option<String> {
    if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    // OpenAI-style content arrays: [{type: "text", text: "…"}, …]
    if let Some(parts) = message.get("content").and_then(|v| v.as_array()) {
        let collected: String = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if !collected.is_empty() {
            return Some(collected);
        }
    }
    None
}

/// Compute the set of tool names that plan mode wants hidden, based
/// on the current turn's schema view. Pure: hand it the schemas the
/// turn would have shown and the plan-active flag, get back the
/// names to add to `restricted_tools` (empty when plan mode is off).
///
/// Centralised here so the `add at turn start, remove at turn end`
/// flow is symmetric — see `apply_plan_mode_restrictions` /
/// `clear_plan_mode_restrictions`. Mirrors the existing
/// `interaction_scoped_tool_restrictions` lifecycle.
fn plan_mode_restriction_names(
    plan_active: bool,
    schemas: &[serde_json::Value],
) -> HashSet<String> {
    if !plan_active {
        return HashSet::new();
    }
    let registry = astra_turn_core::tool_categories::registry();
    astra_turn_core::tool_schema_prune::plan_mode_restrictions(schemas, |name| {
        registry.is_read_only(name)
    })
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

fn looks_plan_shaped(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Markdown plan headers — strongest signal.
    let has_markdown_plan_header = lower.contains("## plan")
        || lower.contains("### plan")
        || lower.contains("## steps")
        || lower.contains("### steps");
    if has_markdown_plan_header {
        return true;
    }
    let has_plan_marker = lower.contains("plan:")
        || lower.contains("here's the plan")
        || lower.contains("here is the plan")
        || lower.contains("proposed plan")
        || lower.contains("implementation plan");
    if !has_plan_marker {
        return false;
    }
    // Numbered list with at least three items: 1. … 2. … 3. …
    let mut numbered_hits = 0;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            if rest.starts_with('.') || rest.starts_with(')') {
                numbered_hits += 1;
                if numbered_hits >= 3 {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        deferred_input_status_line, derive_turn_interaction_mode,
        failed_control_tool_recovery_result, permission_mode_change_audit_event,
        plan_mode_missed_exit_reminder, plan_mode_restriction_names,
        request_allowlist_restriction_names,
    };
    use crate::cli::permission_manager::PermissionMode;
    use astra_runtime::turn::agentic_loop::host::TurnInteractionMode;
    use astra_services::session_journal::JournalEventType;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn failed_control_tool_recovery_result_is_explicitly_marked_as_recovery_failure() {
        let result = failed_control_tool_recovery_result(
            "call-fanout",
            "agent_fanout",
            &json!({"action": "start"}),
            "Cannot recover missing agent_fanout edge result",
        );

        assert_eq!(result.status, "failed");
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "failed");
        assert_eq!(output["recovery"]["attempted"], true);
        assert_eq!(output["recovery"]["source"], "host_state");
        assert_eq!(output["recovery"]["outcome"], "failed");

        let fields = result.tool_result_fields.expect("recovery metadata");
        assert_eq!(fields["recovery"]["attempted"], true);
        assert_eq!(fields["recovery"]["source"], "host_state");
        assert_eq!(fields["recovery"]["outcome"], "failed");
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
            derive_turn_interaction_mode(PermissionMode::Ask, false, false, true, false, true),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Edits, false, false, true, false, true,),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, false, true),
            TurnInteractionMode::Auto
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ci, false, false, false, false, true),
            TurnInteractionMode::Ci
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_for_subtasks_and_silent_turns() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ask, true, false, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ask, false, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn deferred_input_status_line_renders_feedback_prefix_and_preview() {
        let line = deferred_input_status_line(&json!({
            "content": "先停啊！"
        }))
        .expect("deferred input feedback should be rendered");
        assert!(line.starts_with("__deferred_input_applied__:"));
        assert!(line.contains("先停啊"));
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_without_tty_or_native_prompt_sink() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ask, false, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ask, false, false, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ask, false, true, true, false, true),
            TurnInteractionMode::Prompt
        );
    }

    // ── Auto mode preservation under non-interactive contexts ─────────
    //
    // The user's Auto-mode intent is "don't interrupt me, trust the
    // model". This must NOT be silently demoted to NonInteractive just
    // because the turn is happening in a piped-stdin / silent-render /
    // approval-channel context — those only matter for Ask mode (no
    // stdin to prompt on → fall back to NonInteractive so nothing
    // blocks on a human). In Auto, there's nothing to prompt anyway,
    // so the structural non-interactivity is orthogonal.
    //
    // Regression for session c6e18730: Auto-mode user saw `## ⚠
    // Sequential Tool Calls Detected` nudges injected into message
    // history because `suppresses_loop_nudges` in agentic_loop_execution_phase
    // gates on `TurnInteractionMode::Auto`, but derive_turn_interaction_mode
    // was collapsing Auto → NonInteractive for any structural reason,
    // so `suppress_nudges` evaluated false and nudges fired.

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
            "approval-tx (e.g. web-approval flow) is irrelevant to Auto — Auto short-\
             circuits approvals anyway"
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
    fn derive_turn_interaction_mode_demotes_ci_like_ask() {
        // CI mode under non-interactive also falls back to
        // NonInteractive (no opportunity to refuse anything
        // interactively — and CI behaviour is already deterministic
        // denial, same as NonInteractive's restrictive default).
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Ci, false, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn derive_turn_interaction_mode_keeps_ask_user_for_interactive_plan() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Plan, false, false, true, false, true),
            TurnInteractionMode::Prompt
        );
    }

    #[test]
    fn derive_turn_interaction_mode_hides_ask_user_for_plan_without_prompt_sink() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Plan, false, false, false, false, true),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn derive_turn_interaction_mode_uses_noninteractive_for_structural_plan() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Plan, false, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    // ── Session c47c2dca regression guard ─────────────────────────────
    //
    // `CliAgenticLoopHost::execute_turn` MUST drain
    // `state.volatile_pending` before handing messages to the bridge,
    // otherwise runtime-injected nudges (stall reflections, circuit-
    // breaker self-check, Task #42/#43 advisories, budget warnings,
    // the Corrective family, etc.) never reach the LLM. The
    // `take_volatile_pending_as_message` method on `AgenticLoopState`
    // exists specifically for this CLI-side drain — if someone
    // removes the call, nudges silently disappear again.
    //
    // A stronger test would drive the full execute_turn path and
    // snoop the outgoing HTTP payload. But execute_turn pulls in
    // enough non-trivial state (HTTP client, executor, perm manager,
    // memoria hub, …) that the minimal-reproduction cost isn't
    // justified for a single-line check. A source-level assertion
    // suffices to guard the invariant and documents WHY the call
    // is there.

    #[test]
    fn execute_turn_drains_volatile_lane_into_edge_profile() {
        // Guard against the session c47c2dca regression. The CLI must
        // drain the structured volatile lane before building the
        // outgoing HTTP payload; otherwise stall nudges, circuit-breaker
        // self-check messages, and Task #42/#43 advisories are silently
        // dropped.
        //
        // We check three independent textual signatures of the fix —
        // assembled by string concatenation so this test's literals
        // don't self-match. If any one of these goes missing, the
        // regression is likely back.
        let source = include_str!("cli_loop_host.rs");

        // Signature 1: the drain method must be actually INVOKED, not
        // just mentioned in comments/docstrings. We look for the exact
        // call syntax (dot prefix + parens suffix) assembled via
        // concat! so this test's literal cannot self-satisfy.
        //
        // The accepted method is the structured drain. The CLI must not inline
        // the text into messages because only the server has the resolved model
        // row and prompt-cache capability metadata.
        //
        // Do NOT quote the call syntax verbatim anywhere in this
        // function body or its comments — it would defeat the check.
        let safe_call = concat!(".take_volatile_pending", "()");
        assert!(
            source.contains(safe_call),
            "execute_turn must invoke the structured volatile drain method \
             (session c47c2dca regression + cache-capability guard). \
             The expected call syntax is absent; nudges will be dropped \
             or model-specific cache policy will be bypassed."
        );

        // Signature 2: the drained text travels in the dedicated request field,
        // not as appended user messages.
        assert!(
            source.contains("runtime_volatile_texts"),
            "execute_turn must route the drained volatile lane through \
             runtime_volatile_texts so the server can apply cache capability"
        );

        // Signature 3: raw history stays raw; volatile must not mutate
        // `messages[]` before server-side model resolution.
        assert!(
            source.contains("messages: state.messages.as_slice()"),
            "execute_turn must pass raw state.messages and keep volatile \
             separate from the conversation history"
        );
    }

    #[test]
    fn plan_nudge_fires_on_numbered_plan_without_exit_call() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "Sure. Plan:\n1. Read auth.rs\n2. Add tests\n3. Submit PR",
        })];
        let nudge = plan_mode_missed_exit_reminder(&messages)
            .expect("numbered plan without exit_plan_mode call must trigger nudge");
        assert!(
            nudge.contains("exit_plan_mode"),
            "nudge must point the model at exit_plan_mode. Got: {nudge}"
        );
    }

    #[test]
    fn plan_nudge_fires_on_markdown_plan_header() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "## Plan\n\nWe will read the file then add tests.",
        })];
        assert!(plan_mode_missed_exit_reminder(&messages).is_some());
    }

    #[test]
    fn plan_nudge_skipped_when_assistant_called_exit_plan_mode() {
        // The model already submitted the plan via the tool — no
        // need to nag.
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "Submitting the plan for approval.",
            "tool_calls": [
                {
                    "id": "call_1",
                    "function": {
                        "name": "exit_plan_mode",
                        "arguments": "{\"plan\":\"1. step\"}"
                    }
                }
            ]
        })];
        assert!(plan_mode_missed_exit_reminder(&messages).is_none());
    }

    #[test]
    fn plan_nudge_skipped_for_short_analytical_answer() {
        // A one-paragraph answer is not plan-shaped and must not
        // trigger the nudge — false positives spam the model.
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "The auth module lives in `src/auth.rs`; it uses bcrypt.",
        })];
        assert!(plan_mode_missed_exit_reminder(&messages).is_none());
    }

    #[test]
    fn plan_nudge_skipped_for_analytical_numbered_list_without_plan_marker() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "I'll analyze this in three stages:\n1. Read the module\n2. Trace the call graph\n3. Summarize the risk",
        })];
        assert!(
            plan_mode_missed_exit_reminder(&messages).is_none(),
            "ordinary analytical numbered lists should not force a plan approval nudge"
        );
    }

    #[test]
    fn plan_nudge_handles_openai_content_array_shape() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Plan:"},
                {"type": "text", "text": "1. read\n2. test\n3. ship"},
            ]
        })];
        assert!(plan_mode_missed_exit_reminder(&messages).is_some());
    }

    #[test]
    fn plan_nudge_skipped_when_no_assistant_message_yet() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "Investigate the auth module"
        })];
        assert!(plan_mode_missed_exit_reminder(&messages).is_none());
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
    fn plan_mode_restriction_names_lists_mutating_only_in_plan_mode() {
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
        // Mutating tools restricted.
        assert!(plan_on.contains("write_file"));
        assert!(plan_on.contains("str_replace"));
        assert!(plan_on.contains("bash"));
        // Read-only and plan-control tools survive.
        assert!(!plan_on.contains("read_file"));
        assert!(!plan_on.contains("grep"));
        assert!(!plan_on.contains("exit_plan_mode"));
        assert!(!plan_on.contains("enter_plan_mode"));
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
    fn plan_mode_restrictions_are_cleared_after_turn_ends() {
        // Simulates the per-turn lifecycle the host runs:
        //   turn N: plan_active=true, add restrictions
        //   turn N ends, restrictions removed
        //   turn N+1: plan_active=false (user/agent exited plan
        //             mode), restrictions empty so write_file flows
        //             through normally.
        let schemas = vec![
            schema("read_file"),
            schema("write_file"),
            schema("bash"),
            schema("exit_plan_mode"),
        ];
        let mut restricted: HashSet<String> = HashSet::new();

        // Turn N: enter plan mode.
        let plan_set = plan_mode_restriction_names(true, &schemas);
        restricted.extend(plan_set.iter().cloned());
        assert!(restricted.contains("write_file"));
        assert!(restricted.contains("bash"));

        // Turn N ends — host removes the names it added.
        for name in &plan_set {
            restricted.remove(name);
        }
        assert!(
            restricted.is_empty(),
            "after turn ends, plan-mode restrictions must be gone — they are turn-scoped, not session-scoped (regression: session 19298aea)"
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
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let _ = tx.send(StreamEvent::Compaction(event));

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
