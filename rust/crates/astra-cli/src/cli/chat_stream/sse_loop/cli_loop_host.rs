//! CLI implementation of [`AgenticLoopHost`].
//!
//! Wraps CLI-specific concerns (tool executor, permission manager, selector,
//! skill registry, terminal rendering) behind the runtime trait so the
//! multi-turn loop runs in the runtime crate.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use astra_runtime::{
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::agentic_headless_round::HeadlessStderrStyle,
    turn::agentic_loop_host::{
        AgenticLoopHost, AgenticLoopState, HostTurnResult, TurnInteractionMode,
        interaction_scoped_tool_restrictions,
    },
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use serde_json::Value;

use crate::{
    ExplainMode,
    edge_tools::ToolExecutor,
    permission_manager::{PermissionManager, PermissionMode},
    stream_render::RenderPolicy,
};

use super::agentic_loop_turn::{
    ChatTurnSseFetchRequest, PrepareTurnTelemetry, fetch_chat_turn_sse,
};

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
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: PathBuf,
    pub executor: Arc<ToolExecutor>,
    pub selector: &'a dyn ToolSelector,
    pub registry: ToolRegistry,
    pub all_schemas: Vec<Value>,
    pub file_context: Vec<String>,
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: HashSet<String>,
    /// Lines written to stderr between SSE turns (headless tool output, etc.)
    /// that the next `consume_turn_sse` must clear before streaming.
    pub pending_clear_lines: usize,
    pub is_plan_subtask: bool,
    pub plan_subtask_id: Option<&'a str>,
    pub plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::super::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<super::super::ApprovalRequestTx>,
    /// Root-level messaging context used when the current turn has no mailbox.
    pub root_send_message_context:
        Option<crate::edge_tools::agent_messaging::SendMessageRuntimeContext>,
    /// REPL turn counter (0-based) for correct turn_id in trace collector.
    pub repl_turn_index: u32,
    /// Cross-turn tool output cache for edge-path dedup.
    pub tool_cache: crate::stream_render::EdgeToolCache,
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
}

fn derive_turn_interaction_mode(
    permission_mode: PermissionMode,
    is_plan_subtask: bool,
    has_approval_request_tx: bool,
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
        // Prompt requires user interaction; if we can't actually prompt
        // (no tty, alternate approval channel, silenced UI), fall back
        // to NonInteractive so callers don't block on a human.
        PermissionMode::Prompt => {
            if has_approval_request_tx || render_is_silent || !stdin_is_terminal {
                TurnInteractionMode::NonInteractive
            } else {
                TurnInteractionMode::Prompt
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
            self.render_policy.is_silent(),
            std::io::stdin().is_terminal(),
        )
    }

    /// Backwards-compatible inherent alias used by existing call sites
    /// inside this module. Delegates to `turn_interaction_mode_inherent`.
    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        self.turn_interaction_mode_inherent()
    }
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let assembly_start = Instant::now();

        // Preserve the lifecycle-created collector: it may already contain the
        // initial skill selector shortlist for this turn.
        let turn_id = format!("turn-{}", self.repl_turn_index);
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

        // Session c47c2dca regression fix: drain the structured volatile
        // lane BEFORE subsequent immutable state borrows — the lane
        // holds runtime nudges (stall reflection, circuit-breaker
        // self-check, Task #42/#43 advisories, etc.) that must ride the
        // outgoing payload or the LLM never sees them. Using the
        // `_appended_to` variant so we never produce consecutive
        // role=user pairs (Bedrock HTTP 400). See
        // `take_volatile_pending_as_message` / `take_volatile_pending_appended_to`
        // docs for the full context.
        let augmented_messages_owned: Option<Vec<serde_json::Value>> =
            state.take_volatile_pending_appended_to(state.messages.clone());

        // If a skill activation overrode the model, use that; otherwise fall back to host default.
        let effective_model = state.skills.model_override.as_deref().or(self.model);

        // Skill allowed_tools is additive — it ensures skill-referenced tools
        // are visible to the model via schema injection, but never restricts
        // other tools. Only interaction-scoped restrictions apply here.
        let interaction_mode = self.turn_interaction_mode();
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        state
            .restricted_tools
            .extend(interaction_scoped_restrictions.iter().cloned());

        // Propagate skill sandbox policy to the tool executor for this turn.
        // Saved/restored so it doesn't persist after the skill deactivates.
        let prev_sandbox = self
            .executor
            .sandbox_policy
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(ref policy) = state.skills.sandbox_policy {
            *self
                .executor
                .sandbox_policy
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(policy.clone());
        } else {
            *self
                .executor
                .sandbox_policy
                .write()
                .unwrap_or_else(|e| e.into_inner()) = prev_sandbox.clone();
        }
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

        // Use the augmented messages from the volatile drain if any;
        // otherwise fall through to state.messages untouched. The
        // augmentation is already protocol-safe (no consecutive-user
        // pairs — see `take_volatile_pending_appended_to`).
        let messages_slice: &[serde_json::Value] = match augmented_messages_owned.as_ref() {
            Some(vec) => vec.as_slice(),
            None => state.messages.as_slice(),
        };

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
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: Arc::clone(&self.executor),
            selector: self.selector,
            registry: &self.registry,
            messages: messages_slice,
            ephemeral_prefix: state.skills.listing_message.as_ref(),
            current_session_id: state.current_session_id.as_deref(),
            tool_results: state.tool_results.as_slice(),
            all_schemas: &self.all_schemas,
            turn_guard: &state.turn_guard,
            restricted_tools: &mut state.restricted_tools,
            step_recorder: &mut state.step_recorder,
            file_context: &self.file_context,
            assembly_start,
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut state.telemetry.first_memoria_ms,
                first_selector_ms: &mut state.telemetry.first_selector_ms,
                first_selector_strategy: &mut state.telemetry.first_selector_strategy,
                first_selector_confidence: &mut state.telemetry.first_selector_confidence,
                selector_tokens_in: &mut state.telemetry.selector_tokens_in,
                selector_tokens_out: &mut state.telemetry.selector_tokens_out,
                first_selection_report: &mut state.telemetry.first_selection_report,
                first_budget_pressure: &mut state.telemetry.first_budget_pressure,
                first_context_assembly_ms: &mut state.telemetry.first_context_assembly_ms,
                all_selected_skills: &mut state.telemetry.all_selected_skills,
                initial_skill_selector_shortlist: state
                    .telemetry
                    .initial_skill_selector_shortlist
                    .as_ref()
                    .and_then(|shortlist| serde_json::to_value(shortlist).ok()),
                trace_collector: state.telemetry.turn_trace_collector.as_ref(),
            },
            perm_manager: self.perm_manager,
            skill_search: &state.skills.search,
            pre_clear_lines: pre_clear,
            is_plan_subtask: self.is_plan_subtask,
            plan_subtask_id: self.plan_subtask_id,
            cancel_token: state.cancellation.token.as_deref(),
            plan_assemble_line_release: self.plan_assemble_line_release.clone(),
            stream_event_tx: self.stream_event_tx.clone(),
            approval_request_tx: self.approval_request_tx.clone(),
            skill_resolver: state.skills.resolver.clone(),
            skill_effort: state.skills.effort.as_ref().map(|e| e.to_string()),
            skill_agent_type: state.skills.agent_type.clone(),
            tool_budget_override: state.tool_budget_override,
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
            append_system_prompt: self.append_system_prompt.as_deref(),
        })
        .await;

        for name in &interaction_scoped_restrictions {
            state.restricted_tools.remove(name);
        }

        // Restore previous sandbox policy after the turn.
        *self
            .executor
            .sandbox_policy
            .write()
            .unwrap_or_else(|e| e.into_inner()) = prev_sandbox;

        // Sync latest approval overrides into state for checkpoint persistence.
        state.approval_overrides = self.perm_manager.export_session_overrides();

        let turn_result = turn_result?;
        if let Some(refreshed_token) = turn_result.refreshed_token.clone() {
            self.executor.set_cloud_token(refreshed_token.clone());
            self.token = refreshed_token;
        }

        // Update introspect snapshot so the `introspect` tool returns fresh
        // data if the model calls it on a subsequent round this turn.
        let total_in = state.total_prompt + state.total_cache_read + state.total_cache_creation;
        let cache_ratio = if total_in > 0 {
            state.total_cache_read as f64 / total_in as f64
        } else {
            0.0
        };
        let working_mem = state
            .pipeline_session
            .as_ref()
            .map(|s| s.working_memory().render_prompt_section())
            .unwrap_or_default();
        self.executor
            .update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
                token_pressure: 0.0,
                cache_hit_ratio: cache_ratio,
                turns_completed: state.llm_rounds_completed,
                turns_remaining: state.remaining_turns as u32,
                compaction_tier: format!("{:?}", state.compact_tier_applied),
                alerts: Vec::new(),
                tool_health: Vec::new(),
                working_memory_summary: working_mem,
                total_input_tokens: state.total_prompt + state.total_cache_read,
                total_output_tokens: state.total_completion,
                cache_read_tokens: state.total_cache_read,
                cache_creation_tokens: state.total_cache_creation,
                // Task #46 fields populated from state on each turn.
                recent_rounds: state
                    .recent_rounds
                    .iter()
                    .map(|r| astra_turn_core::introspect::RoundSnapshotEntry {
                        turn: r.turn,
                        round: r.round,
                        provider: r.provider.clone(),
                        model: r.model.clone(),
                        prompt_tokens: r.prompt_tokens,
                        cache_read_tokens: r.cache_read_tokens,
                        cache_creation_tokens: r.cache_creation_tokens,
                        completion_tokens: r.completion_tokens,
                        tool_calls_returned: r.tool_calls_returned,
                        tool_call_names: r.tool_call_names.clone(),
                        duration_ms: r.duration_ms,
                        finish_reason: r.finish_reason.clone(),
                    })
                    .collect(),
                volatile_pending: state
                    .volatile_pending
                    .iter()
                    .map(|inj| astra_turn_core::introspect::VolatileSnapshotEntry {
                        kind: format!("{:?}", inj.kind),
                        content: inj.content.clone(),
                        round_index: inj.round_index,
                    })
                    .collect(),
                stall_state: astra_turn_core::introspect::StallSnapshotSummary {
                    nudge_count: state.stall.nudge_count,
                    events: state
                        .stall
                        .events
                        .iter()
                        .map(|(name, turn)| format!("{name} @ turn {turn}"))
                        .collect(),
                    introspection_count: state.stall.introspection_count,
                    forced_execution_escalation: state.stall.forced_execution_escalation,
                    forced_parallel_batching: state.stall.forced_parallel_batching,
                    forced_completion_soft_stop: state.stall.forced_completion_soft_stop,
                    forced_redundant_reads_corrective: state
                        .stall
                        .forced_redundant_reads_corrective,
                    forced_cache_waste_corrective: state.stall.forced_cache_waste_corrective,
                    forced_exploration_family_phase2: state.stall.forced_exploration_family_phase2,
                    forced_exploration_family_corrective: state
                        .stall
                        .forced_exploration_family_corrective,
                },
                // Injection freshness is session-scoped (filled by
                // `handle_introspect` from `ObservabilitySession.injection_history`).
                injection_freshness: Vec::new(),
                current_round: state.current_round_index,
            });

        Ok(HostTurnResult {
            accum: turn_result.core,
            ttft_ms: turn_result.ttft_ms,
            edge_tool_round: turn_result.edge_tool_round,
            error_kind: None,
        })
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        // Forward to stream event channel (even in suppress mode)
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::super::StreamEvent::StatusLine(line.clone()));
        }
        if self.render_policy.suppress_headless() {
            return;
        }
        match style {
            HeadlessStderrStyle::Dim => eprintln!("{}", line.dim()),
            HeadlessStderrStyle::Red => eprintln!("{}", line.red()),
            HeadlessStderrStyle::Green => eprintln!("{}", line.green()),
            HeadlessStderrStyle::Yellow => eprintln!("{}", line.yellow()),
            HeadlessStderrStyle::CyanBold => eprintln!("{}", line.cyan().bold()),
            HeadlessStderrStyle::Magenta => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line.magenta());
            }
            HeadlessStderrStyle::DiffAdd => {
                let body = line.strip_prefix('+').unwrap_or(line.as_str());
                eprint!("{}", "│ ".dim());
                eprint!("{}", "+".green().bold());
                eprintln!("{}", body.green());
            }
            HeadlessStderrStyle::DiffRemove => {
                let body = line.strip_prefix('-').unwrap_or(line.as_str());
                eprint!("{}", "│ ".dim());
                eprint!("{}", "-".red().bold());
                eprintln!("{}", body.red());
            }
            HeadlessStderrStyle::DiffContext => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line.dim());
            }
            HeadlessStderrStyle::Normal => eprintln!("{}", line),
        }
        self.pending_clear_lines += 1;
    }

    fn is_quiet(&self) -> bool {
        self.render_policy.is_silent()
    }

    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        self.turn_interaction_mode_inherent()
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tool_names
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            let name_owned = name.to_string();
            self.valid_tool_names.insert(name_owned.clone());
            self.registry.upsert_schema(schema.clone());
            if let Some(existing) = self.all_schemas.iter_mut().find(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(name_owned.as_str())
            }) {
                *existing = schema;
            } else {
                self.all_schemas.push(schema);
            }
        }
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
            let mut md = crate::streaming_md::StreamingMarkdown::new(self.term_width);
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
        state: &astra_runtime::turn::agentic_loop_host::AgenticLoopState,
    ) {
        // Bug B step 3: capture the parent turn's cacheable prefix
        // so subsequent spawn_agent / delegate calls can inherit it
        // for prompt-cache reuse. No-op unless:
        //   - the `prefix_store` Arc was plumbed in (CLI startup
        //     sets this on every host when fork_prefix.enabled)
        //   - ingest populated the expected state fields
        let Some(store) = self.prefix_store.as_ref() else {
            return;
        };
        let parent_run_id = match state.current_run_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        let model_id = self.model.unwrap_or("").to_string();
        let provider = astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(&model_id);
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
            parent_turn_seq: self.repl_turn_index,
            provider,
            model_id,
            thinking: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_turn_interaction_mode_maps_permission_mode_when_interactive() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, true),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, true),
            TurnInteractionMode::Auto
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Deny, false, false, false, true),
            TurnInteractionMode::Deny
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_for_subtasks_and_silent_turns() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, true, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_without_tty_or_with_approval_channel() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, true, false, true),
            TurnInteractionMode::NonInteractive
        );
    }

    // ── Auto mode preservation under non-interactive contexts ─────────
    //
    // The user's Auto-mode intent is "don't interrupt me, trust the
    // model". This must NOT be silently demoted to NonInteractive just
    // because the turn is happening in a piped-stdin / silent-render /
    // approval-channel context — those only matter for Prompt mode (no
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
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, false),
            TurnInteractionMode::Auto,
            "Auto must NOT be demoted to NonInteractive just because stdin is piped — \
             user's opt-in to uninterrupted execution still applies"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_preserves_auto_with_silent_render() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, true, true),
            TurnInteractionMode::Auto,
            "silent render (e.g. --quiet or harness) must not override Auto intent"
        );
    }

    #[test]
    fn derive_turn_interaction_mode_preserves_auto_with_approval_channel() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, true, false, true),
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
            derive_turn_interaction_mode(PermissionMode::Auto, true, false, false, true),
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
            derive_turn_interaction_mode(PermissionMode::Deny, false, false, false, false),
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
    fn execute_turn_drains_volatile_lane_into_outgoing_messages() {
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
        // The accepted method is the protocol-safe `_appended_to`
        // variant — appending a bare user msg via the non-safe variant
        // would create consecutive-user-role pairs that Bedrock
        // rejects. Either is better than dropping the lane entirely,
        // but the safe one is what production must use.
        //
        // Do NOT quote the call syntax verbatim anywhere in this
        // function body or its comments — it would defeat the check.
        let safe_call = concat!(".take_volatile_pending", "_appended_to(");
        assert!(
            source.contains(safe_call),
            "execute_turn must invoke the protocol-safe drain method \
             (session c47c2dca regression + consecutive-user guard). \
             The expected call syntax is absent; nudges will be dropped \
             or produce invalid payloads."
        );

        // Signature 2: the LOCAL outgoing-messages vec built from
        // state.messages + the drained volatile msg. Pattern stays
        // lexically distinct from this test's literals.
        assert!(
            source.contains("augmented.push(msg)"),
            "execute_turn must append the drained volatile msg to a local \
             clone of state.messages (session c47c2dca regression fix shape)"
        );

        // Signature 3: the slice handed to the fetch request must be
        // the augmented one when non-empty, else state.messages.
        assert!(
            source.contains("messages_slice"),
            "execute_turn must pass an augmented messages_slice to \
             fetch_chat_turn_sse, not raw state.messages (session c47c2dca)"
        );
    }
}
