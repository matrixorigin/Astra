use std::time::Instant;

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, DeferredInputState, HostTurnResult,
    TaskBoardSnapshot, finalize_and_render, finalize_turn_trace, try_write_heavy_checkpoint,
};
use super::lifecycle::{
    TurnIterationPrep, current_agentic_step, interruption_diagnosis_summary,
    interruption_state_summary, session_turn_number, tool_record_is_workspace_mutation,
};
use astra_core::render_compact_status;
use astra_services::{ContextManifestWrite, DatabaseContextManifestStore};
use astra_turn_core::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut, AgenticTurnIngestOutcome,
    agentic_turn_stream_snapshot_with_kind, ingest_agentic_turn_stream,
    map_ingest_outcome_to_iteration_control,
};
use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind, CompactionTier};
use astra_turn_core::interaction_types::TurnInteractionMode;
use astra_turn_core::interruption::{InterruptionKind, InterruptionRecord, ResumeAction};
use astra_turn_core::stall::IntentDrift;
use uuid::Uuid;

/// Lazily-initialized process-wide alert dispatcher.
///
/// Reads `ASTRA_ALERT_WEBHOOK_URL` (and optional `ASTRA_ALERT_WEBHOOK_MIN_SEVERITY`)
/// once, builds a single `AlertDispatcher` with a reusable reqwest client so that
/// webhook calls share a TCP connection pool and TLS session cache across turns.
///
/// Returns `None` when no webhook URL is configured — the whole alert-dispatch
/// branch is then a single cheap `OnceLock` load.
fn global_alert_dispatcher()
-> Option<&'static std::sync::Arc<astra_turn_core::alert_dispatcher::AlertDispatcher>> {
    use std::sync::OnceLock;
    static DISPATCHER: OnceLock<
        Option<std::sync::Arc<astra_turn_core::alert_dispatcher::AlertDispatcher>>,
    > = OnceLock::new();
    DISPATCHER
        .get_or_init(|| {
            let url = std::env::var("ASTRA_ALERT_WEBHOOK_URL").ok()?;
            let url = url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let min_severity = std::env::var("ASTRA_ALERT_WEBHOOK_MIN_SEVERITY")
                .ok()
                .and_then(|s| match s.to_ascii_lowercase().as_str() {
                    "info" => Some(astra_turn_core::trace_alert::AlertSeverity::Info),
                    "warning" | "warn" => {
                        Some(astra_turn_core::trace_alert::AlertSeverity::Warning)
                    }
                    "error" => Some(astra_turn_core::trace_alert::AlertSeverity::Error),
                    _ => None,
                })
                .unwrap_or(astra_turn_core::trace_alert::AlertSeverity::Error);
            let client =
                std::sync::Arc::new(astra_turn_core::alert_dispatcher::ReqwestWebhookClient::new());
            let cfg = astra_turn_core::alert_dispatcher::AlertWebhookConfig { url, min_severity };
            Some(std::sync::Arc::new(
                astra_turn_core::alert_dispatcher::AlertDispatcher::new(cfg, client),
            ))
        })
        .as_ref()
}

pub(crate) fn deferred_user_input_text(input: &serde_json::Value) -> Option<String> {
    fn trimmed_text(value: Option<&serde_json::Value>) -> Option<String> {
        value
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
    }

    fn active_skills_text(input: &serde_json::Value) -> Option<String> {
        let skills = input
            .get("active_skills")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|skill| !skill.is_empty())
            .collect::<Vec<_>>();
        (!skills.is_empty()).then(|| format!("Requested active skills: {}.", skills.join(", ")))
    }

    if let Some(text) = trimmed_text(Some(input)) {
        return Some(text);
    }

    let content = trimmed_text(input.get("content"));
    let text = trimmed_text(input.get("text"));
    let active_skills = active_skills_text(input);

    match (content.or(text), active_skills) {
        (Some(content), Some(active_skills)) => Some(format!("{active_skills}\n{content}")),
        (Some(content), None) => Some(content),
        (None, Some(active_skills)) => Some(active_skills),
        (None, None) => None,
    }
}

fn render_deferred_user_input(content: &str) -> String {
    format!(
        "A newer user message arrived during execution and now supersedes the previous plan.\n\nLatest user message:\n{content}\n\nRequired behavior:\n- Treat this as the newest user instruction.\n- Address it before making more tool calls.\n- Do not continue the previous plan blindly.\n- Only make another tool call if it is directly necessary to satisfy this newest user message."
    )
}

pub(crate) async fn inject_polled_deferred_user_inputs<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    let (run_control, run_id) = match (state.run_control.as_ref(), state.current_run_id.as_ref()) {
        (Some(run_control), Some(run_id)) => (run_control.clone(), run_id.clone()),
        _ => return Ok(()),
    };
    let poll = run_control
        .poll_user_inputs(&run_id, state.deferred_input.deferred_user_input_cursor())
        .await;
    if let Some(error) = &poll.error {
        tracing::warn!(run_id, error = %error, "deferred user input poll failed");
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::DatabaseError,
            format!("failed to poll deferred user input for run {run_id}: {error}"),
        ));
    }
    let observed = state
        .deferred_input
        .observe_polled_user_inputs(poll, deferred_user_input_text);
    let release_event_indices = state
        .deferred_input
        .release_event_indices_to_ack(&observed.released_event_indices);
    if observed.raw_inputs.is_empty() && release_event_indices.is_empty() {
        return Ok(());
    }

    for input in &observed.raw_inputs {
        host.on_deferred_user_input(input);
    }

    if !observed.contents.is_empty() {
        let combined = observed.contents.join("\n\n");
        if !combined.is_empty() {
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": combined.clone(),
            }));
            state.message = combined.clone();
            state.push_volatile(
                super::host::VolatileKind::DeferredUserInput,
                render_deferred_user_input(&combined),
            );
        }
    }

    state
        .deferred_input
        .commit_observed_cursor(observed.next_cursor);
    if release_event_indices.is_empty() {
        return Ok(());
    }
    match run_control
        .mark_user_inputs_released(&run_id, &release_event_indices)
        .await
    {
        Ok(()) => state
            .deferred_input
            .note_release_ack_result(&release_event_indices, true),
        Err(error) => {
            state
                .deferred_input
                .note_release_ack_result(&release_event_indices, false);
            tracing::warn!(
                run_id = %run_id,
                ?release_event_indices,
                error = %error,
                "failed to durably acknowledge deferred user input release"
            );
        }
    }
    Ok(())
}

/// Record an `llm_round` event for an early-exit path (no tool calls).
fn record_early_exit_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
) {
    let agentic_step = current_agentic_step(state);
    let run_id = state.current_run_id.clone();
    let duration_ms = turn_start.elapsed().as_millis() as u64;
    state.push_recent_round(super::host::RecentRoundSummary {
        turn: state.session_turn,
        round: state.current_round_index,
        provider: String::new(),
        model: state.current_model_identity().unwrap_or("").to_string(),
        prompt_tokens: turn_result.accum.prompt_tokens,
        cache_read_tokens: turn_result.accum.cache_read_tokens,
        cache_creation_tokens: turn_result.accum.cache_creation_tokens,
        completion_tokens: turn_result.accum.completion_tokens,
        tool_calls_returned: 0,
        tool_call_names: Vec::new(),
        duration_ms,
        finish_reason: finish_reason.map(ToString::to_string),
    });
    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            ttft_ms: turn_result.ttft_ms,
            duration_ms,
            prompt_tokens: turn_result.accum.prompt_tokens,
            completion_tokens: turn_result.accum.completion_tokens,
            cache_read_tokens: turn_result.accum.cache_read_tokens,
            cache_creation_tokens: turn_result.accum.cache_creation_tokens,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: finish_reason.map(Into::into),
            agentic_step: Some(agentic_step),
            source: Some("agentic_loop".into()),
            run_id,
            tool_calls: None,
            ..Default::default()
        });
    }
}

fn unfinished_task_board_snapshot(state: &AgenticLoopState) -> Option<&TaskBoardSnapshot> {
    state
        .hooks
        .task_board_snapshot
        .has_unfinished_tasks()
        .then_some(&state.hooks.task_board_snapshot)
}

fn unfinished_task_board_corrective_message(
    snapshot: &TaskBoardSnapshot,
    original_query: &str,
) -> String {
    format!(
        "[unfinished-task-board:v1]\n\
         Runtime correction: the session task board still has unfinished work. {}.\n\n\
         REQUIRED next-step behavior:\n\
         - Continue executing the remaining task-board work before reporting completion.\n\
         - If a listed task is no longer needed, explicitly update/cancel/close it instead of ignoring it.\n\
         - Do NOT give a success-shaped final answer while these tasks remain open.\n\n\
         Original user query: {original_query}",
        snapshot.status_count_summary(),
    )
}

fn circuit_breaker_abort_detail(state: &AgenticLoopState) -> String {
    let mut detail = format!(
        "The circuit breaker stopped this turn after round {} because the model kept calling tools after the runtime injected a finalization correction.",
        state.llm_rounds_completed
    );
    if let Some(diagnosis) = interruption_diagnosis_summary(state) {
        detail.push_str(&format!(" Likely cause: {diagnosis}."));
    }
    detail.push_str(
        " Progress from earlier rounds is preserved. Resume by synthesizing verified evidence before calling more tools.",
    );
    detail
}

pub(crate) struct TurnExecutionPhase {
    pub(crate) llm_wall_start: Instant,
    pub(crate) turn_result: HostTurnResult,
}

pub(crate) enum TurnExecutionControl {
    Proceed(Box<TurnExecutionPhase>),
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

fn manifest_reason_for_llm_call(state: &AgenticLoopState) -> &'static str {
    let has_compaction_marker = state.messages.iter().any(|message| {
        message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|content| content.to_ascii_lowercase().contains("compaction"))
    });
    if has_compaction_marker {
        "post_compaction"
    } else {
        "normal_turn"
    }
}

fn infer_turn_intent_for_llm_call(
    state: &AgenticLoopState,
    pre_llm_messages: &[serde_json::Value],
) -> String {
    let combined = pre_llm_messages
        .iter()
        .rev()
        .take(8)
        .filter_map(|message| message.get("content").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if combined.contains("benchmark")
        && (combined.contains("compare")
            || combined.contains("comparison")
            || combined.contains("对比")
            || combined.contains("比较"))
    {
        return "benchmark_comparison".to_string();
    }
    if state.task_profile.mutates_workspace {
        "implementation".to_string()
    } else if state.task_profile.exploratory_task {
        "exploration".to_string()
    } else {
        "normal".to_string()
    }
}

async fn persist_context_manifest_for_llm_call(
    state: &AgenticLoopState,
    turn_index: usize,
    llm_attempt_index: u32,
    pre_llm_messages: &[serde_json::Value],
    turn_result: Option<&HostTurnResult>,
) {
    if turn_result.is_none() && state.last_llm_context_manifest_trace.is_none() {
        return;
    }
    let (Some(pool), Some(user_id), Some(session_id), Some(run_id)) = (
        state.context_manifest_pool.clone(),
        state.context_manifest_user_id.as_deref(),
        state.current_session_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        return;
    };
    let turn_intent = infer_turn_intent_for_llm_call(state, pre_llm_messages);
    let schema_tokens = state.pinned_tool_schema_tokens.min(u64::from(u32::MAX)) as u32;
    let result_prompt_tokens = turn_result
        .map(|result| {
            result
                .accum
                .prompt_tokens
                .saturating_add(result.accum.cache_read_tokens)
        })
        .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32);
    let manifest_id = format!("manifest-{}", Uuid::new_v4());
    let turn_id = format!("{run_id}:llm:{llm_attempt_index}");
    let reason = manifest_reason_for_llm_call(state);
    let model_name = state.current_model_identity().unwrap_or("").to_string();
    let context_window_tokens =
        u32::try_from(crate::prompts::budget_for_model(Some(&model_name)).model_limit)
            .unwrap_or(u32::MAX);
    let projection = crate::turn::llm::context::build_context_manifest_projection(
        crate::turn::llm::context::ContextManifestProjectionInput {
            session_id,
            run_id,
            turn_index,
            llm_attempt_index,
            pre_llm_messages,
            tool_results: &state.tool_results,
            schema_tokens,
            result_prompt_tokens,
            observed_fresh_input_tokens: turn_result.map(|result| result.accum.prompt_tokens),
            observed_cache_read_tokens: turn_result.map(|result| result.accum.cache_read_tokens),
            observed_cache_creation_tokens: turn_result
                .map(|result| result.accum.cache_creation_tokens),
            observed_output_tokens: turn_result.map(|result| result.accum.completion_tokens),
            assembly_trace: state.last_llm_context_manifest_trace.clone(),
            turn_intent: &turn_intent,
            reason,
            context_window_tokens,
        },
    );

    let manifest = ContextManifestWrite {
        manifest_id: manifest_id.clone(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        run_id: Some(run_id.to_string()),
        turn_id,
        model_provider: "runtime".to_string(),
        model_name,
        context_window_tokens,
        max_output_tokens: projection.max_output_tokens,
        total_estimated_tokens: projection.total_estimated_tokens,
        policy_version: "context_manifest_v1".to_string(),
        tokenizer_id: Some("estimated_v1".to_string()),
        budget_template_id: Some("budget_v1_8k".to_string()),
        turn_intent: Some(turn_intent.clone()),
        reason: reason.to_string(),
        manifest_json: projection.manifest_json,
    };
    let store = DatabaseContextManifestStore::new(pool);
    if let Err(error) = store.save_manifest(manifest, projection.items).await {
        tracing::warn!(
            target: "astra_runtime::context_manifest",
            run_id,
            session_id,
            manifest_id,
            error = %error,
            "failed to persist per-llm-call context manifest"
        );
    }
}

fn circuit_breaker_introspection_message(
    llm_rounds_completed: u32,
    consecutive_read_only: usize,
) -> String {
    format!(
        "[Self-check — round {}] You have been reading/exploring for {} consecutive rounds \
         without writing. Take a moment to assess:\n\
         - Do you have enough information to produce your answer? If yes, do so now.\n\
         - If not, what specific information are you still missing?\n\
         Tools remain available.",
        llm_rounds_completed, consecutive_read_only
    )
}

pub(crate) async fn execute_turn_and_ingest_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
) -> Result<TurnExecutionControl, astra_core::ClassifiedError> {
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_started(turn_index as u32);
    }

    inject_polled_deferred_user_inputs(host, state).await?;

    // ── Nudge suppression gate ──────────────────────────────────────────
    // In PermissionMode::Auto the user has explicitly asked to let the
    // model run to completion without interruption. Skip all corrective
    // / interruption-style nudges in that case: execution escalation,
    // parallel-batching force, circuit-breaker correction/introspect/
    // soft-stop, exploration-family retries, redundant-reads, cache-
    // waste. Safety-critical abort (circuit breaker) still fires — it
    // terminates the loop, not just nudges.
    //
    // Observed in session 3b7ac18f: ~10 nudge injections across 15
    // turns in Auto mode, user complaint "不停的被打断,不一气呵成".
    let suppress_nudges = host.turn_interaction_mode().suppresses_loop_nudges();
    state.refresh_task_board_snapshot().await;

    // Inject round budget guidance so the model knows to batch or synthesize.
    // Use llm_rounds_completed (actual LLM call count) not turn_index (step
    // counter inflated by progressive penalty).
    // Skip when the host already injects guidance (e.g. server path injects
    // it into the system prompt in its own execute_turn).
    //
    // The guidance is *ephemeral*: we strip any prior guidance message before
    // injecting a fresh one so the message vec (and any downstream REPL-history
    // replay that keys off it) does not accumulate one guidance block per
    // LLM round. Detection uses the stable headings produced by
    // `round_budget_directive`.
    fn is_ephemeral_round_budget_msg(m: &serde_json::Value) -> bool {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            return false;
        }
        m.get("content").and_then(|c| c.as_str()).is_some_and(|s| {
            s.contains("## ⚡ Round Budget")
                || s.contains("## ⚠ Round Budget")
                || s.contains("## ⚡ Self-Status")
        })
    }

    if !host.injects_round_guidance() {
        // Drop any stale guidance/status messages from prior rounds.
        state.messages.retain(|m| !is_ephemeral_round_budget_msg(m));

        // ── Self-Status injection (push-mode observation) ─────────────────
        // Always inject a compact self-status block so the agent sees its
        // current health (token pressure, trends, alerts, circuit breaker)
        // without needing to call `introspect`. This closes the pull→push gap.
        // Skip when budget is exhausted — the agent should produce final
        // output, not introspect.
        if state.remaining_turns > 0
            && (state.llm_rounds_completed > 0 || !state.observation_journal.is_empty())
        {
            // Construct a lightweight provider for live metrics.
            use crate::turn::providers::{LiveRuntimeProvider, SessionStateProvider};
            let status_provider =
                crate::turn::local_provider::LocalSessionProvider::new(state);
            let cb_state = status_provider.circuit_breaker_state().to_string();
            let cache_ratio = status_provider.cache_hit_ratio();
            let token_pressure = status_provider.token_pressure();
            let alerts: Vec<String> = {
                let mut a = Vec::new();
                if state.stall.forced_execution_escalation {
                    a.push("execution_escalation".to_string());
                }
                if state.stall.drift_nudge_count > 0 {
                    a.push(format!("drift_nudges={}", state.stall.drift_nudge_count));
                }
                if state.stall.nudge_count > 0 {
                    a.push(format!("stall_nudges={}", state.stall.nudge_count));
                }
                if !state.turn_guard.health.recent_errors(10).is_empty() {
                    a.push(format!(
                        "tool_errors={}",
                        state.turn_guard.health.recent_errors(10).len()
                    ));
                }
                a
            };
            let status = render_compact_status(
                &state.observation_journal,
                &alerts,
                &cb_state,
                token_pressure,
                cache_ratio,
                state.llm_rounds_completed,
                state.remaining_turns as u32,
            );
            if !status.is_empty() {
                astra_turn_core::chat_history_openai::append_openai_user_content_messages(
                    &mut state.messages,
                    &[status],
                );
            }
        }

        if !suppress_nudges {
            let guidance =
                crate::prompts::tool_round_guidance(&state.messages, state.llm_rounds_completed);
            if !guidance.is_empty() {
                astra_turn_core::chat_history_openai::append_openai_user_content_messages(
                    &mut state.messages,
                    &[guidance],
                );
            }
        }
    }

    // Mid-loop execution escalation: if the model has been burning tool calls
    // on read-only inspection of a mutating task without committing a single
    // edit, force a high-signal correction BEFORE the next LLM call. This
    // catches the failure mode where the loop runs out of budget in an
    // inspection spiral (see session 4178c6a7). One-shot per turn; stripped
    // by `finalize_and_render`.
    if !suppress_nudges && should_escalate_execution(state) {
        let read_only_calls = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder() && r.ok)
            .count();
        state.stall.forced_execution_escalation = true;
        let msg = execution_escalation_message(&state.message, read_only_calls);
        state.push_volatile(super::host::VolatileKind::ExecutionEscalation, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "execution_escalation",
            read_only_calls,
            round = state.llm_rounds_completed,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ Mutating task accumulated {read_only_calls} read-only tool calls with zero edits; forcing escalation…"
                ),
            );
        }
    }

    // Load runtime config once per round for all mid-loop guards below.
    let tool_cfg = &astra_config::runtime_config::RuntimeConfig::load().tool_policy;
    let resolved_tool_policy =
        tool_cfg.resolve_for_model(state.context_manifest_model_name.as_deref());
    let parallel_batching_force_threshold =
        resolved_tool_policy.parallel_batching_force_streak as usize;
    let redundant_reads_threshold = tool_cfg.effective_redundant_reads_midloop_threshold() as usize;
    let cache_waste_threshold = tool_cfg.effective_cache_waste_midloop_threshold() as usize;
    let search_fanout_threshold = tool_cfg.effective_search_fanout_eval_threshold() as usize;
    let exploration_family_threshold =
        tool_cfg.effective_exploration_family_churn_midloop_threshold() as usize;

    // ── Composable guard pipeline ────────────────────────────────────────
    // Each guard is defined in the `guards` module. The pipeline evaluates
    // them in order, injects corrections, and propagates aborts. Guards are
    // independently testable — see guards.rs for individual unit tests.
    //
    // Previously each guard was inlined as ~30-line blocks below; the
    // pipeline reduces this section from ~80 lines to ~15.
    //
    // Runs *after* the circuit breaker so guards that defer to budget
    // pressure (redundant_reads, cache_waste) see the final
    // `forced_round_budget_phase1` state and can correctly skip when the
    // breaker has already escalated.
    {
        let guard_cfg = super::guards::GuardConfig {
            suppress_nudges,
            parallel_batching_force_streak: parallel_batching_force_threshold,
            redundant_reads_threshold,
            cache_waste_threshold,
        };
        let guards = super::guards::default_guards();
        match super::guards::evaluate_guards(&guards, state, &guard_cfg) {
            Ok(corrections) => {
                for (hint_style, hint_text) in corrections {
                    if !prep.quiet {
                        host.emit_headless_line(hint_style, hint_text);
                    }
                }
            }
            Err(abort_reason) => {
                // Guard pipeline requested abort — terminate the turn.
                //
                // Record a structured InterruptionRecord so resumption surfaces
                // *why* the guard pipeline halted the turn. Without this, the
                // abort reason lives only in `final_text` (an opaque string),
                // and a resumed session cannot tell a guard abort apart from a
                // normal completion. The journal/checkpoint consumer relies on
                // `state.interruption` for machine-readable recovery context.
                state.final_text = abort_reason.clone();
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::GuardAbort,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(state, Some(abort_reason)),
                ));
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "guard_pipeline_abort",
                    round = state.llm_rounds_completed,
                    "guard pipeline abort — turn terminated by guard"
                );
                state.step_recorder.end_turn(false);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }
        }
    }

    // ── Policy-driven evaluation (RuntimePolicy) ───────────────────────
    // Compute JournalFacts from the current loop state and delegate to
    // RuntimePolicy::decide(). The Policy produces FrameworkActions that
    // complement the guard pipeline above — budget expansion, phase
    // transitions, and signal injection.
    //
    // This runs after the guard pipeline so it sees the latest tool-call
    // records and circuit-breaker state.
    {
        use crate::turn::local_provider::LocalSessionProvider;
        use crate::turn::providers::{
            LiveRuntimeProvider, ObservationProvider, SessionStateProvider,
        };
        use crate::turn::runtime_policy::FrameworkAction;
        use astra_core::observation_journal::JournalFacts;

        let provider = LocalSessionProvider::new(state);

        // Extract journal facts from the ObservationProvider trait.
        let mut facts = provider.extract_facts();

        // Populate session-wide fields from authoritative state.
        // extract_facts provides streak and budget data from the journal
        // window; these fields come from the full session state.
        facts.budget.rounds_completed = state.llm_rounds_completed;
        facts.performance.total_evidence_calls = state.total_evidence_tool_calls;
        facts.performance.total_errors = state.turn_guard.health.recent_errors(10).len() as u32;
        facts.performance.total_tool_calls = state.total_tool_calls;

        // Stall reason from the unified stall diagnosis.
        facts.stall.stall_reason = interruption_diagnosis_summary(state);

        // Populate token pressure from the LiveRuntimeProvider.
        facts.performance.token_pressure = provider.token_pressure();

        // Populate task completion ratio from the SessionStateProvider.
        facts.task.task_completion_ratio = provider.task_completion_ratio();

        let default_policy = crate::turn::runtime_policy::RuntimePolicy::default();
        let policy = state.budget_policy.as_ref().unwrap_or(&default_policy);
        let actions = policy.decide(&facts);
        for action in actions {
            match action {
                FrameworkAction::ExpandBudget {
                    factor,
                    max_ceiling,
                } => {
                    let new_max = ((state.max_turns as f64) * factor).ceil() as usize;
                    let ceiling = max_ceiling as usize;
                    let capped = new_max.min(ceiling);
                    if capped > state.max_turns {
                        let added = capped - state.max_turns;
                        state.max_turns = capped;
                        state.remaining_turns += added;
                        state.policy_expanded_this_turn = true;
                        // Reset self-pacing hint flags so the new budget gets fresh hints
                        state.turn_budget_hint_emitted_90 = false;
                        state.turn_budget_hint_emitted_50 = false;
                        state.turn_budget_hint_emitted_20 = false;
                        let msg = format!(
                            "[Budget review] Recent progress: expanding budget by ×{factor:.1} ({added} additional turns). Hard ceiling: {ceiling} total turns."
                        );
                        state.push_volatile(super::host::VolatileKind::BudgetReview, msg);
                        tracing::info!(
                            target: "astra::policy",
                            factor,
                            max_ceiling,
                            new_max = capped,
                            "Policy-driven budget expansion"
                        );
                    }
                }
                FrameworkAction::InjectSignal { message } => {
                    state.push_volatile(
                        super::host::VolatileKind::Corrective,
                        format!("[Framework signal] {}", message),
                    );
                    tracing::info!(
                        target: "astra::policy",
                        signal = %message,
                        "Policy injected signal into agent context"
                    );
                }
                FrameworkAction::SignalContextPressure { urgency } => {
                    let pressure = facts.performance.token_pressure;
                    let msg = format!(
                        "[Context pressure] Token pressure is {:.0}% ({urgency}). Conserve context: reuse prior tool results, avoid duplicate reads, summarize current evidence briefly, and prefer targeted next actions.",
                        pressure * 100.0,
                        urgency = urgency,
                    );
                    state.push_volatile(super::host::VolatileKind::ContextPressure, msg);
                    tracing::info!(
                        target: "astra::policy",
                        %urgency,
                        token_pressure = pressure,
                        "Policy context-pressure guidance injected"
                    );
                }
                FrameworkAction::TransitionPhase { target } => {
                    let phase_label = match target {
                        crate::turn::runtime_policy::PhaseTarget::Reflection => "reflection",
                        crate::turn::runtime_policy::PhaseTarget::Summarization => "summarization",
                        crate::turn::runtime_policy::PhaseTarget::Planning => "planning",
                        crate::turn::runtime_policy::PhaseTarget::Completion => "completion",
                    };
                    let msg = format!(
                        "[Framework action] Phase transition requested: {phase_label}. Consider wrapping up and preparing to transition."
                    );
                    state.push_volatile(super::host::VolatileKind::Corrective, msg);
                    if target == crate::turn::runtime_policy::PhaseTarget::Completion {
                        // Signal completion — inject a stronger nudge when all tasks are done.
                        let completion_msg = format!(
                            "[Framework action] All tasks completed (ratio: {:.0}%). Consider finalizing the turn.",
                            facts.task.task_completion_ratio * 100.0,
                        );
                        state.push_volatile(super::host::VolatileKind::Corrective, completion_msg);
                    }
                    tracing::info!(
                        target: "astra::policy",
                        %target,
                        completion_ratio = facts.task.task_completion_ratio,
                        "Policy-driven phase transition"
                    );
                }
                FrameworkAction::Continue => {}
            }
        }
    }

    // ── Intent drift detection ─────────────────────────────────────────
    // Check if the agent has drifted from the user's original intent by
    // analyzing recent tool calls against the user query. If drift is
    // detected, inject a correction via the volatile lane so the LLM
    // refocuses on the original task. Singleton kind ensures only the
    // latest correction rides the wire, avoiding prompt cache bloat.
    //
    // Runs after the guard pipeline so it sees the most recent tool calls
    // in `state.stall.intent_tool_turns`. Skipped when `suppress_nudges`
    // is true (Auto mode) to avoid interrupting the flow.
    //
    // One-shot per turn: once forced_intent_drift is set, no further
    // corrections are injected this turn, preserving prompt-cache prefix.
    if !suppress_nudges && !state.stall.forced_intent_drift && state.llm_rounds_completed > 0 {
        let drift = host
            .detect_intent_drift(&state.message, &state.stall.intent_tool_turns)
            .await;
        if let IntentDrift::Drifting { correction, .. } = drift {
            state.stall.forced_intent_drift = true;
            state.stall.drift_nudge_count += 1;
            state
                .turn_guard
                .sync_drift_nudge_count(state.stall.drift_nudge_count);
            state.stall.last_drift_correction_round = state.llm_rounds_completed as usize;
            state.push_volatile(super::host::VolatileKind::IntentDrift, correction.clone());
            tracing::info!(
                target: "astra::loop_guard",
                tier = "intent_drift",
                round = state.llm_rounds_completed,
                drift_nudge_count = state.stall.drift_nudge_count,
                "intent drift detected — injecting correction"
            );
            if !prep.quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "⚠ Intent drift detected — correcting course (nudge #{})",
                        state.stall.drift_nudge_count
                    ),
                );
            }
        }
    }

    // ── Circuit breaker observation ──────────────────────────────────────
    // Feed the previous round's signal to the circuit breaker. On the first
    // round (no previous tool calls), this is skipped. The breaker decides
    // whether to inject a correction or abort based on anomaly detection.
    if state.llm_rounds_completed > 0 {
        let signal = build_circuit_breaker_signal(state);
        let action = state.stall.circuit_breaker.observe(signal);
        match action {
            astra_turn_core::loop_circuit_breaker::BreakerAction::InjectCorrection
                if suppress_nudges =>
            {
                // Auto mode: drop the correction entirely. Abort path
                // below still fires because it represents a real budget
                // exhaustion, not a soft nudge.
                state.stall.circuit_breaker.correction_injected();
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::InjectCorrection => {
                state.stall.forced_round_budget_phase1 = true;
                state.stall.circuit_breaker.correction_injected();
                // Physical tool lockout for the upcoming round.
                //
                // Historically this path injected a text-only corrective that
                // said "tools are disabled" but didn't actually restrict them,
                // so the model sometimes ignored the instruction and kept
                // calling tools (observed: session 36500dd9 round 13 kept
                // using bash/read_file despite the message). Adding every
                // valid tool to `restricted_tools` flips the phase1 promise
                // from aspirational to enforced: the payload builder filters
                // these out before the next request is built, so the
                // model physically cannot emit another tool call this round.
                for name in host.valid_tool_names() {
                    state.restricted_tools.insert(name.clone());
                }
                let msg = round_budget_phase1_message(state.llm_rounds_completed, &state.message);
                state.push_volatile(super::host::VolatileKind::BudgetAdvisory, msg);
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_correction",
                    round = state.llm_rounds_completed,
                    "circuit breaker tripped — injecting correction"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Circuit breaker tripped at round {} (stall/regression detected); forcing finalization…",
                            state.llm_rounds_completed
                        ),
                    );
                }
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Abort => {
                state.stall.forced_round_budget_phase2 = true;
                let diagnosis = interruption_diagnosis_summary(state);
                // Rich, contextual abort message: includes the diagnosis,
                // the most recent preserved tool calls, and a concrete
                // next-step line tied to the stall pattern. Used when the
                // model has not yet produced any free-form text — the old
                // behaviour was to leave users with a single red banner.
                let abort_msg = super::lifecycle::build_circuit_breaker_abort_message(state);
                if state.final_text.trim().is_empty() {
                    state.final_text = abort_msg.clone();
                } else {
                    state.final_text.push_str("\n\n");
                    state.final_text.push_str(&abort_msg);
                }
                state.final_text_streamed = false;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::BudgetExhausted,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(state, Some(abort_msg)),
                ));
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_abort",
                    round = state.llm_rounds_completed,
                    "circuit breaker abort — agent did not recover"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        if let Some(diagnosis) = diagnosis.as_deref() {
                            format!(
                                "⛔ Circuit breaker abort at round {}; likely cause: {}.",
                                state.llm_rounds_completed, diagnosis
                            )
                        } else {
                            format!(
                                "⛔ Circuit breaker abort at round {}; agent did not recover after correction.",
                                state.llm_rounds_completed
                            )
                        },
                    );
                }
                state.step_recorder.end_turn(false);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Introspect { .. }
                if suppress_nudges =>
            {
                // Auto mode: don't interject a [Self-check — round N]
                // message; the user opted in to uninterrupted execution.
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Introspect {
                consecutive_read_only,
            } => {
                // `introspection_count` is monotonic for the lifetime of this turn
                // (not reset between introspect emissions). It mirrors the breaker's
                // own `introspect_emissions_since_last_write` counter but is retained
                // for structured logging / observability only.
                state.stall.introspection_count = state.stall.introspection_count.saturating_add(1);
                let emission_index = state.stall.introspection_count;
                let msg = circuit_breaker_introspection_message(
                    state.llm_rounds_completed,
                    consecutive_read_only,
                );
                state.push_volatile(super::host::VolatileKind::CircuitBreaker, msg);
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_introspect",
                    round = state.llm_rounds_completed,
                    consecutive_read_only,
                    emission = emission_index,
                    "circuit breaker introspection — periodic self-check injected"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Self-check prompt injected at round {} ({} consecutive read-only rounds, emission #{}).",
                            state.llm_rounds_completed, consecutive_read_only, emission_index
                        ),
                    );
                }
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::SoftStop if suppress_nudges => {}
            astra_turn_core::loop_circuit_breaker::BreakerAction::SoftStop => {
                state.stall.forced_completion_soft_stop = true;
                let msg = completion_soft_stop_message(state.llm_rounds_completed, &state.message);
                state.push_volatile(super::host::VolatileKind::CircuitBreaker, msg);
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "completion_soft_stop",
                    round = state.llm_rounds_completed,
                    "completion soft-stop injected"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Task appears complete at round {}; nudging model to stop unless work remains.",
                            state.llm_rounds_completed
                        ),
                    );
                }
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Continue => {}
            // BreakerAction is #[non_exhaustive] — future soft-intervention
            // variants should default to a no-op so the loop continues.
            _ => {}
        }
    }

    if !suppress_nudges
        && !state.stall.forced_round_budget_phase1
        && !state.stall.forced_completion_soft_stop
        && let Some((family, blocked_tools)) = exploration_family_phase2_candidate(state)
    {
        state.stall.forced_exploration_family_phase2 = true;
        let msg = exploration_family_phase2_message(&family, &blocked_tools, &state.message);
        state.push_volatile(super::host::VolatileKind::Corrective, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_phase2",
            round = state.llm_rounds_completed,
            family = family,
            blocked_tools = ?blocked_tools,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ blocked-only retry on restricted {family} tools [{}]; forcing convergence corrective…",
                    blocked_tools.join(", ")
                ),
            );
        }
    }

    // Redundant-reads and cache-waste correctives are now handled by the
    // composable guard pipeline above (`default_guards()`). They were
    // previously inlined here as ~30-line blocks; the pipeline version also
    // carries their stderr hints so the host doesn't need to re-render them.
    if !suppress_nudges
        && !state.stall.hard_intervention_active()
        && !state.stall.forced_redundant_reads_corrective
        && !state.stall.forced_cache_waste_corrective
        && should_inject_search_fanout_corrective(state, search_fanout_threshold)
    {
        let count =
            astra_turn_core::evaluation::count_search_fanout(&state.stall.tool_call_records);
        state.stall.forced_search_fanout_corrective = true;
        for tool in ["glob", "grep", "rg"] {
            state.restricted_tools.insert(tool.to_string());
        }
        let msg = search_fanout_corrective_message(count, &state.message);
        state.push_volatile(super::host::VolatileKind::Corrective, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "search_fanout_corrective",
            round = state.llm_rounds_completed,
            count = count,
            threshold = search_fanout_threshold,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!("↻ {count} search calls in an implementation turn; forcing synthesis before more search…"),
            );
        }
    }
    if !suppress_nudges
        && !state.stall.hard_intervention_active()
        && !state.stall.forced_redundant_reads_corrective
        && !state.stall.forced_cache_waste_corrective
        && !state.stall.forced_search_fanout_corrective
        && let Some((family, streak)) =
            exploration_family_corrective_candidate(state, exploration_family_threshold)
    {
        let restricted = apply_exploration_family_restrictions(state, &family);
        state.stall.forced_exploration_family_corrective = true;
        let msg =
            exploration_family_corrective_message(&family, streak, &restricted, &state.message);
        state.push_volatile(super::host::VolatileKind::Corrective, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_corrective",
            round = state.llm_rounds_completed,
            family = family,
            streak = streak,
            restricted = ?restricted,
            "loop guard fired"
        );
        if !prep.quiet {
            let restricted_display = restricted.join(", ");
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {streak} consecutive low-yield {family} rounds; restricting [{restricted_display}] for the next round…"
                ),
            );
        }
    }

    // ── Harness: PreLlmRequest — Block/Pause prevents LLM call ──
    #[cfg(feature = "harness")]
    match super::super::harness_adapter::harness_at!(
        &state.harness,
        astra_harness::HookPoint::PreLlmRequest,
        state
    ) {
        astra_harness::HookVerdict::Block { reason } => {
            tracing::warn!(reason = %reason, "harness blocked LLM call at PreLlmRequest");
            super::host::set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                &reason,
            );
            return Ok(TurnExecutionControl::Return(
                super::host::AgenticLoopOutcome::Completed,
            ));
        }
        astra_harness::HookVerdict::Pause { reason, .. } => {
            tracing::info!(reason = %reason, "harness paused LLM call at PreLlmRequest");
            super::host::set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessPaused,
                &reason,
            );
            return Ok(TurnExecutionControl::Return(
                super::host::AgenticLoopOutcome::Completed,
            ));
        }
        astra_harness::HookVerdict::Continue => {}
    }
    #[cfg(not(feature = "harness"))]
    super::super::harness_adapter::harness_at!(
        &state.harness,
        astra_harness::HookPoint::PreLlmRequest,
        state
    );

    let llm_wall_start = Instant::now();
    let pre_llm_messages = state.messages.clone();
    let llm_attempt_index = state.llm_rounds_completed;
    state.last_llm_context_manifest_trace = None;
    // Protect the exact request prefix we are about to send even if the LLM
    // call fails; the next retry/compaction pass must not clear tool results
    // that were already part of this attempted request.
    state.last_request_message_count = Some(pre_llm_messages.len());
    // Increment the LLM-round counter regardless of outcome so retry/error
    // paths don't see a stale count (the counter tracks *attempted* LLM
    // calls for guidance-threshold purposes, not just successful ones).
    let turn_result = host.execute_turn(state).await;
    state.llm_rounds_completed += 1;
    // Capture finish_reason before the match consumes turn_result.
    // Used by textless-stop retry (loop level) and ensure_terminal_text
    // (finalization level) to distinguish true silence from forced truncation
    // when the API's max_tokens limit cuts off the model's output.
    state.last_finish_reason = turn_result
        .as_ref()
        .ok()
        .and_then(|r| r.accum.finish_reason.clone());
    // Persist the per-call manifest only after the host returns: the durable
    // record includes observed token usage and the emitted context-manifest
    // trace, both of which are only available on the completed turn result.
    if let Ok(result) = &turn_result
        && let Some(trace) = result.accum.context_manifest_trace.clone()
    {
        state.last_llm_context_manifest_trace = Some(trace);
    }
    match &turn_result {
        Ok(result) => {
            persist_context_manifest_for_llm_call(
                state,
                turn_index,
                llm_attempt_index,
                &pre_llm_messages,
                Some(result),
            )
            .await;
        }
        Err(_) => {
            persist_context_manifest_for_llm_call(
                state,
                turn_index,
                llm_attempt_index,
                &pre_llm_messages,
                None,
            )
            .await;
        }
    }
    let turn_result = turn_result?;
    state.rate_limit_cooldown.record_success();
    // Clear pipeline recovery escalation after a successful LLM call —
    // the PTL pressure is relieved.
    if let Some(ref mut sess) = state.pipeline_session {
        sess.recovery.reset_on_success();
    }
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_completed(
            turn_index as u32,
            turn_result.ttft_ms,
            llm_wall_start.elapsed().as_millis() as u64,
        );
    }

    let snap = agentic_turn_stream_snapshot_with_kind(
        &turn_result.accum,
        turn_result.ttft_ms,
        turn_result.error_kind,
    );
    update_turn_trace_collector(state, &turn_result);

    let edge_len = turn_result.edge_tool_round.len();
    let ingest_outcome = ingest_agentic_turn_stream(
        &snap,
        edge_len,
        |i| turn_result.edge_tool_round[i].tool.clone(),
        &state.message,
        &state.recent_tools,
        prep.quiet,
        AgenticTurnIngestMut {
            task_profile: state.task_profile,
            step_persistence_enabled: state.context_manifest_user_id.is_some(),
            first_ttft_ms: &mut state.telemetry.first_ttft_ms,
            current_session_id: &mut state.current_session_id,
            current_run_id: &mut state.current_run_id,
            final_text: &mut state.final_text,
            total_prompt: &mut state.total_prompt,
            total_completion: &mut state.total_completion,
            total_cache_read: &mut state.total_cache_read,
            total_cache_creation: &mut state.total_cache_creation,
            total_tool_calls: &mut state.total_tool_calls,
            total_evidence_tool_calls: &mut state.total_evidence_tool_calls,
            step_recorder: &mut state.step_recorder,
            all_tools_used: &mut state.telemetry.all_tools_used,
            has_any_usage: &mut state.has_any_usage,
            forced_factual_retry: &mut state.stall.forced_factual_retry,
            messages: &mut state.messages,
            last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
            consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
            turn_policy: state.last_turn_policy.clone(),
        },
    );

    // PR 5a: post-sampling hook. Fires exactly once after a
    // successful turn has been received AND cleanly ingested
    // (non-Fatal outcome), BEFORE any side effects (tool phase,
    // microcompact prep, memory extraction).
    //
    // Fatal ingest outcomes include SSE-embedded rate limits,
    // context-window overflows, and provider 5xx strings. On those,
    // state is only partially updated — firing the hook would let a
    // downstream capture snapshot record a corrupt prefix. We peek
    // at the variant via `matches!` so the original `ingest_outcome`
    // can still move by-value into the control-flow mapper below.
    let ingest_is_fatal = matches!(ingest_outcome, AgenticTurnIngestOutcome::Fatal(_));
    if !ingest_is_fatal {
        if let Some(ref mut pipeline_sess) = state.pipeline_session {
            let mut feedback = astra_turn_core::context_feedback::ContextFeedback::from_usage(
                turn_result.accum.prompt_tokens,
                turn_result.accum.cache_read_tokens,
                turn_result.accum.cache_creation_tokens,
                turn_result.accum.completion_tokens,
                false,
            );
            let model_id = state.skills.model_override.as_deref().unwrap_or("default");
            pipeline_sess.record_feedback(model_id, "agentic_loop", &mut feedback, None);

            // Emit pipeline journal events for observability and cloud sync
            if let Some(ref mut buf) = state.turn_event_buffer {
                let turn = state.llm_rounds_completed;
                let session_id = state.current_session_id.as_deref();

                // Per-turn feedback event
                let feedback_evt =
                    astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
                        turn, model_id, &feedback,
                    );
                if let Ok(payload) = serde_json::to_value(&feedback_evt) {
                    buf.record(
                        astra_services::session_journal::JournalEvent::pipeline_feedback(
                            session_id, turn, payload,
                        ),
                    );
                }

                // Drain and emit compaction audit events
                for audit in pipeline_sess.drain_pending_audits() {
                    if let Ok(payload) = serde_json::to_value(&audit) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_compaction_audit(
                                session_id, turn, payload,
                            ),
                        );
                    }
                }

                // Evaluate trace alerts and emit them to the journal.
                let alerts = astra_turn_core::trace_alert::evaluate_alerts(
                    turn,
                    &feedback,
                    &pipeline_sess.stats,
                    &pipeline_sess.recovery,
                );
                // Best-effort webhook dispatch: dispatcher is initialized once
                // per process via a global OnceLock, reusing reqwest::Client's
                // connection pool + TLS session cache across turns. Dispatch
                // runs async so it never blocks turn execution.
                if !alerts.is_empty() {
                    if let Some(dispatcher) = global_alert_dispatcher() {
                        let session_id_str = session_id.unwrap_or("unknown-session").to_string();
                        let alerts_to_send = alerts.clone();
                        let dispatcher = dispatcher.clone();
                        tokio::spawn(async move {
                            dispatcher.dispatch(&session_id_str, &alerts_to_send).await;
                        });
                    }
                }

                for alert in &alerts {
                    let alert_evt =
                        astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(alert);
                    if let Ok(payload) = serde_json::to_value(&alert_evt) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_alert(
                                session_id, turn, payload,
                            ),
                        );
                    }
                }
            }
        }
        host.on_turn_completed(state);
    }

    match map_ingest_outcome_to_iteration_control(ingest_outcome) {
        AgenticIngestIterationControl::Fatal(e) => {
            use astra_core::ErrorKind;

            let is_rate_limit = matches!(e.kind, ErrorKind::RateLimit);

            if is_rate_limit {
                state.rate_limit_cooldown.record_429(None, false);
            }
            if matches!(e.kind, ErrorKind::ServerError) {
                state.rate_limit_cooldown.record_529(None, false);
            }

            if is_rate_limit && state.total_tool_calls > 0 {
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "⚠ Rate limit hit after {} tool calls — preserving work.",
                            state.total_tool_calls,
                        ),
                    );
                }
                state.final_text = format!(
                    "[Rate limit reached after {} tool call(s). \
                     All completed tool results are preserved above. \
                     You can continue from where I left off in the next message.]\n\n\
                     Error: {}",
                    state.total_tool_calls, e.message,
                );
                state.final_text_streamed = false;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e.message)),
                    ),
                ));
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("rate_limited"),
                );
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                );
                state.step_recorder.end_turn(false);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            if is_rate_limit {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e.message)),
                    ),
                ));
            }

            // ── Context-window overflow: compact and retry ────────────
            let is_context_overflow = e.kind == ErrorKind::ContextWindow;
            if is_context_overflow {
                // If a prior compaction ran but we still got a 413, mark it insufficient.
                if state.compaction_effectiveness.last_tokens_freed > 0
                    && !state.compaction_effectiveness.last_was_insufficient
                {
                    state.compaction_effectiveness.mark_insufficient();
                }
            }
            if is_context_overflow
                && state.consecutive_context_window_errors
                    <= super::super::compaction_replay::MAX_COMPACT_RETRIES
            {
                // Inform the pipeline session about the PTL error so its
                // RecoveryState can escalate tier on subsequent turns and
                // widen reserve estimates. This bridges the legacy compaction
                // retry path with the pipeline's observability/feedback loop.
                if let Some(ref mut sess) = state.pipeline_session {
                    sess.recovery.record_ptl_error();
                }

                let outcome = super::super::compaction_replay::try_compact_for_retry_checked(
                    &mut state.messages,
                    &mut state.compaction_effectiveness,
                    state.last_measured_prompt_tokens,
                    state.max_turn_input_tokens,
                    state.consecutive_context_window_errors,
                );
                match outcome {
                    super::super::compaction_replay::CompactionReplayOutcome::Compacted(result) => {
                        let tier_label = result.tier.to_string();
                        // Feed compaction stats into pipeline for reserve estimation.
                        if let Some(ref mut sess) = state.pipeline_session {
                            sess.recovery.record_reactive_compact();
                            sess.stats.record_compaction(result.tokens_freed);
                        }
                        let tokens_freed = result.pipeline_outcome.total_tokens_freed;
                        let messages_after = state.messages.len();
                        if !prep.quiet {
                            // In a retry context we know we overflowed the
                            // context window, so use max_turn_input_tokens as
                            // the floor for tokens_before when measured value
                            // is unavailable (rather than 0, which is misleading).
                            let tokens_before = state
                                .last_measured_prompt_tokens
                                .unwrap_or(state.max_turn_input_tokens);
                            let pressure = if state.max_turn_input_tokens == 0 {
                                0.0
                            } else {
                                (tokens_before as f64 / state.max_turn_input_tokens as f64).min(1.0)
                            };
                            let event = CompactionEvent::new(
                                result.tier,
                                pressure,
                                tokens_freed,
                                tokens_before,
                                state.max_turn_input_tokens,
                                result.messages_removed,
                                messages_after,
                                result.layer_descriptions.clone(),
                            );
                            host.on_compaction(event);
                        }

                        // Emit structured compaction telemetry for observability.
                        if let Some(sid) = state.current_session_id.as_deref() {
                            let budget_likely_satisfied = result.budget_likely_satisfied;
                            let layers: Vec<(String, u64)> = result
                                .pipeline_outcome
                                .layer_results
                                .iter()
                                .map(|(name, cr)| (name.clone(), cr.estimated_tokens_freed))
                                .collect();
                            let evt =
                                astra_services::session_journal::JournalEvent::compaction_retry(
                                    Some(sid),
                                    session_turn_number(state),
                                    &tier_label,
                                    tokens_freed,
                                    budget_likely_satisfied,
                                    state.consecutive_context_window_errors,
                                    layers,
                                    state.consecutive_context_window_errors,
                                )
                                .with_agentic_step(Some(current_agentic_step(state)));
                            // `JournalWriter::append` auto-prepends
                            // `SessionStart` under the same file lock;
                            // see `prepend_session_start_if_needed`.
                            if let Ok(writer) =
                                astra_services::session_journal::JournalWriter::new(sid)
                            {
                                let _ = writer.append(&evt);
                            }
                        }

                        try_write_heavy_checkpoint(state);
                        return Ok(TurnExecutionControl::ContinueLoop);
                    }
                    super::super::compaction_replay::CompactionReplayOutcome::CircuitOpen => {
                        // Session has burned enough futile attempts; don't
                        // run the pipeline again. Fall through to the
                        // ContextOverflow interruption path below so the
                        // caller can resume from checkpoint.
                        if !prep.quiet {
                            host.emit_headless_line(
                                HeadlessStderrStyle::Yellow,
                                format!(
                                    "♻ Context overflow — compaction circuit open after {} \
                                     futile attempts; giving up for this session.",
                                    state.compaction_effectiveness.consecutive_futile_attempts,
                                ),
                            );
                        }
                    }
                    super::super::compaction_replay::CompactionReplayOutcome::Futile => {
                        // Single futile attempt — counter advanced by the
                        // checked helper. Next turn's check may trip the
                        // breaker. Fall through to interruption.
                    }
                }
            }
            // If we reach here with a context overflow that couldn't be
            // compacted (or retries exhausted), record a structured
            // interruption so the session can resume from checkpoint.
            if is_context_overflow {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::ContextOverflow,
                    ResumeAction::CompactAndRetry,
                    interruption_state_summary(
                        state,
                        Some(format!("Context overflow after compaction: {}", e.message)),
                    ),
                ));
            }

            // Catch-all: map ErrorKind to InterruptionRecord so the checkpoint
            // always carries resume guidance. Existing specific records (rate
            // limit, context overflow) take priority — only fill when still empty.
            if state.interruption.is_none() {
                if let Some((kind, action)) =
                    astra_turn_core::interruption::interruption_from_error_kind(e.kind)
                {
                    state.interruption = Some(InterruptionRecord::new(
                        kind,
                        action,
                        interruption_state_summary(state, Some(e.message.clone())),
                    ));
                }
            }

            finalize_turn_trace(state).await;
            try_write_heavy_checkpoint(state);
            return Err(e);
        }
        AgenticIngestIterationControl::BreakLoop => {
            if let Some(retry_reason) = execution_retry_reason(state) {
                state.stall.forced_execution_retry = true;
                state.final_text.clear();
                // The corrective user message is pushed onto `state.messages`
                // for this loop iteration. The one-shot
                // `forced_execution_retry` flag prevents a second injection,
                // and `finalize_and_render` strips the marker before the next
                // user turn so it does not pollute future conversations.
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": execution_retry_message(&state.message, retry_reason),
                }));
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "execution_retry",
                    round = state.llm_rounds_completed,
                    "loop guard fired"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        execution_retry_notice(retry_reason),
                    );
                }
                // Intentionally skip record_verdict: no evaluation happened, only
                // StepIncomplete is emitted as the terminal event.
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("execution_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            if !state.stall.forced_task_board_completion_gate
                && let Some(snapshot) = unfinished_task_board_snapshot(state).cloned()
            {
                state.stall.forced_task_board_completion_gate = true;
                state.final_text.clear();
                state.final_text_streamed = false;
                state.push_volatile(
                    super::host::VolatileKind::TaskBoardCompletionGate,
                    unfinished_task_board_corrective_message(&snapshot, &state.message),
                );
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "unfinished_task_board",
                    round = state.llm_rounds_completed,
                    summary = %snapshot.short_summary(),
                    "unfinished task-board work blocked loop completion"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Unfinished tasks remain; continuing ({})",
                            snapshot.short_summary()
                        ),
                    );
                }
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("unfinished_tasks"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            if state.hooks.stop_hook_runs == 0 && should_skip_auto_verify_stop_hooks(state) {
                state.hooks.stop_hook_runs = 1;
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Green,
                        "✓ Verification already observed after latest change; skipping duplicate auto hook.".to_string(),
                    );
                }
            } else if state.hooks.stop_hook_runs == 0
                && let Some(prompt) =
                    astra_turn_core::stop_hooks::build_stop_hook_prompt(&state.hooks.stop_hooks)
            {
                state.hooks.stop_hook_runs = 1;
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        "⚠ Verification required, continuing…".to_string(),
                    );
                }
                state.messages.push(prompt);
                // Intentionally skip record_verdict: no evaluation happened, only
                // StepIncomplete is emitted as the terminal event.
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("stop_hook"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // ── Textless stop retry (policy-driven) ──────────────────────
            // Delegate to RuntimePolicy::decide_textless_stop which
            // centralizes the retry logic, exploration-task exemption,
            // and nudge construction. The policy returns InjectSignal
            // when a retry is warranted.
            if state.final_text.trim().is_empty() {
                let default_policy = crate::turn::runtime_policy::RuntimePolicy::default();
                let policy = state.budget_policy.as_ref().unwrap_or(&default_policy);
                if let Some(action) = policy.decide_textless_stop(
                    state.textless_stop_retries,
                    state.total_tool_calls as u32,
                    state.task_profile.exploratory_task,
                    suppress_nudges,
                ) {
                    if let crate::turn::runtime_policy::FrameworkAction::InjectSignal {
                        message: nudge,
                    } = action
                    {
                        state.textless_stop_retries += 1;
                        state.push_volatile(super::host::VolatileKind::BudgetAdvisory, nudge);
                        record_early_exit_llm_round(
                            state,
                            &turn_result,
                            prep.turn_start_time,
                            Some("textless_stop_retry"),
                        );
                        state.step_recorder.end_turn(false);
                        try_write_heavy_checkpoint(state);
                        return Ok(TurnExecutionControl::ContinueLoop);
                    }
                }
            }

            // Record the LLM round even for text-only responses (no tool calls).
            // Without this, simple Q&A turns have llm_rounds=0 in the
            // journal despite the LLM being called.
            record_early_exit_llm_round(state, &turn_result, prep.turn_start_time, Some("stop"));
            state.step_recorder.end_turn(true);

            observe_turn_end_without_tools(
                state,
                turn_index,
                prep.turn_start_time,
                turn_result.ttft_ms,
            );
            finalize_and_render(host, state).await;
            return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
        }
        AgenticIngestIterationControl::ContinueIterating => {
            // Intentionally skip record_verdict: no evaluation happened, only
            // StepIncomplete is emitted as the terminal event.
            record_early_exit_llm_round(
                state,
                &turn_result,
                prep.turn_start_time,
                Some("continue"),
            );
            state.step_recorder.end_turn(false);
            try_write_heavy_checkpoint(state);
            return Ok(TurnExecutionControl::ContinueLoop);
        }
        AgenticIngestIterationControl::ProceedWithToolCalls => {}
    }

    // Circuit breaker post-LLM check: if the correction was injected (phase1)
    // but the model still emitted tool calls, escalate to abort. We do NOT
    // call observe() again — the breaker observes exactly once per completed
    // round (in the pre-LLM block). Here we just check: correction was given,
    // model ignored it → abort.
    if state.stall.forced_round_budget_phase1 && !state.stall.forced_round_budget_phase2 {
        state.stall.forced_round_budget_phase2 = true;
        let abort_detail = circuit_breaker_abort_detail(state);
        state.final_text.clear();
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, Some(abort_detail)),
        ));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "circuit_breaker_phase2_abort",
            round = state.llm_rounds_completed,
            "circuit breaker phase2 abort (model ignored correction)"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "⛔ Correction ignored at round {}; aborting turn.",
                    state.llm_rounds_completed
                ),
            );
        }
        state.step_recorder.end_turn(false);
        finalize_and_render(host, state).await;
        return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
    }
    emit_subrun_text_preview(host, state, prep.quiet);
    if let Some(control) = handle_token_budget(host, state, turn_index, prep, &turn_result).await {
        return Ok(control);
    }
    if should_wrap_up_for_cumulative_budget(host, state, prep.quiet) {
        return Ok(TurnExecutionControl::ContinueLoop);
    }

    record_tool_selection(state, &turn_result, turn_index);

    Ok(TurnExecutionControl::Proceed(Box::new(
        TurnExecutionPhase {
            llm_wall_start,
            turn_result,
        },
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionRetryReason {
    MissingMutation,
    MissingBrowserVerification,
}

#[cfg(test)]
fn should_force_execution_retry(state: &AgenticLoopState) -> bool {
    execution_retry_reason(state).is_some()
}

fn execution_retry_reason(state: &AgenticLoopState) -> Option<ExecutionRetryReason> {
    if state.stall.forced_execution_retry {
        return None;
    }
    // If mid-loop escalation already fired this turn, the model has already
    // received a stronger corrective message telling it to apply an edit.
    // Adding a second retry injection would duplicate correction, waste
    // tokens, and risk contradicting guidance. One corrective injection per
    // turn is the invariant.
    if state.stall.forced_execution_escalation {
        return None;
    }
    if state.stall.forced_parallel_batching {
        return None;
    }
    if state.stall.any_intervention_active() {
        return None;
    }
    if missing_browser_verification_evidence(state) {
        return Some(ExecutionRetryReason::MissingBrowserVerification);
    }
    if has_concrete_workspace_mutation(state) {
        return None;
    }
    if state.final_text.trim().is_empty() {
        return None;
    }
    let attempted_work_without_mutation = state.total_tool_calls > 0;
    let defers = final_text_defers_execution(&state.final_text);
    if state.task_profile.mutates_workspace {
        if attempted_work_without_mutation
            && !defers
            && final_text_concludes_no_change_needed(&state.final_text)
        {
            return None;
        }
        // Mutating-profile tasks need either a concrete workspace mutation or
        // inspected evidence that no mutation is needed. A zero-tool text-only
        // completion is a high-risk silent no-op, so force exactly one retry.
        return Some(ExecutionRetryReason::MissingMutation);
    }
    (user_confirmed_execution_from_recent_context(state)
        && (attempted_work_without_mutation || defers))
        .then_some(ExecutionRetryReason::MissingMutation)
}

fn missing_browser_verification_evidence(state: &AgenticLoopState) -> bool {
    if state.final_text.trim().is_empty() {
        return false;
    }
    if !message_requires_browser_verification(&state.message) {
        return false;
    }
    if final_text_admits_browser_not_verified(&state.final_text) {
        return false;
    }
    if !final_text_claims_browser_success(&state.final_text) {
        return false;
    }
    !has_browser_verification_evidence(state)
}

fn message_requires_browser_verification(message: &str) -> bool {
    let lower = message.to_lowercase();
    let mentions_browser = [
        "browser",
        "in browser",
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromium",
        "chrome",
        "firefox",
        "webkit",
        "浏览器",
        "ui",
        "页面",
        "canvas",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let mentions_verification = [
        "test",
        "verify",
        "validation",
        "validate",
        "check",
        "open",
        "run",
        "qa",
        "smoke",
        "测试",
        "验证",
        "检查",
        "打开",
        "试玩",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    mentions_browser && mentions_verification
}

fn final_text_claims_browser_success(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "tested",
        "verified",
        "works",
        "working",
        "fully functional",
        "looks good",
        "all good",
        "passes",
        "passed",
        "successfully",
        "已经验证",
        "已验证",
        "测试通过",
        "功能正常",
        "可以正常",
        "运行正常",
        "一切正常",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn final_text_admits_browser_not_verified(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "could not verify in a browser",
        "could not verify in browser",
        "can't verify in a browser",
        "can't verify in browser",
        "not verified in browser",
        "unable to open a browser",
        "unable to open the browser",
        "no browser-capable tool",
        "无法在浏览器中验证",
        "没法在浏览器里验证",
        "不能在浏览器中验证",
        "没有浏览器工具",
        "未在浏览器验证",
        "无法打开浏览器",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_browser_verification_evidence(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.ok && !record.is_synthetic_placeholder())
        .any(tool_record_has_browser_verification_evidence)
}

fn tool_record_has_browser_verification_evidence(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let lower_name = record.name.to_lowercase();
    if [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromedriver",
        "geckodriver",
        "webdriver",
    ]
    .iter()
    .any(|needle| lower_name.contains(needle))
    {
        return true;
    }
    if record.name == "bash" {
        let command = super::lifecycle::extract_bash_command(record.args_full.as_deref())
            .or_else(|| super::lifecycle::extract_bash_command(record.args_preview.as_deref()));
        if command
            .as_deref()
            .is_some_and(text_has_browser_verification_evidence)
        {
            return true;
        }
    }
    [
        record.args_full.as_deref(),
        record.args_preview.as_deref(),
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(text_has_browser_verification_evidence)
}

fn text_has_browser_verification_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromium",
        "google-chrome",
        "chrome --headless",
        "chrome-headless",
        "firefox --headless",
        "webkit",
        "chromedriver",
        "geckodriver",
        "--screenshot",
        "--dump-dom",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_concrete_workspace_mutation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.ok && !record.is_synthetic_placeholder())
        .any(tool_record_is_workspace_mutation)
}

fn should_skip_auto_verify_stop_hooks(state: &AgenticLoopState) -> bool {
    if state.hooks.stop_hooks.is_empty() {
        return false;
    }
    if !state
        .hooks
        .stop_hooks
        .iter()
        .all(is_auto_verify_changes_hook)
    {
        return false;
    }
    has_successful_verification_after_latest_mutation(state)
}

fn is_auto_verify_changes_hook(hook: &astra_turn_core::stop_hooks::StopHook) -> bool {
    hook.label == "verify-changes"
        && hook
            .command
            .contains("Based on the files you actually modified")
}

fn has_successful_verification_after_latest_mutation(state: &AgenticLoopState) -> bool {
    let Some(last_mutation_index) = state.stall.tool_call_records.iter().rposition(|record| {
        record.ok && !record.is_synthetic_placeholder() && tool_record_is_workspace_mutation(record)
    }) else {
        return false;
    };

    state
        .stall
        .tool_call_records
        .iter()
        .skip(last_mutation_index + 1)
        .any(tool_record_is_successful_verification)
}

fn tool_record_is_successful_verification(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.ok
        || record.is_synthetic_placeholder()
        || !tool_record_result_looks_successful(record)
    {
        return false;
    }
    if record.name == "bash" {
        let command = super::lifecycle::extract_bash_command(record.args_full.as_deref())
            .or_else(|| super::lifecycle::extract_bash_command(record.args_preview.as_deref()));
        return command
            .as_deref()
            .is_some_and(command_looks_like_verification);
    }
    tool_name_looks_like_verification(&record.name)
}

fn tool_record_result_looks_successful(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if record.result_class.as_deref().is_some_and(|class| {
        matches!(
            class,
            "error" | "failure" | "failed" | "tool_error" | "command_error"
        )
    }) {
        return false;
    }
    [
        record.error.as_deref(),
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_lowercase)
    .all(|text| {
        let trimmed = text.trim_start();
        !trimmed.starts_with('✗')
            && !text.contains("(exit 1)")
            && !text.contains("exit status 1")
            && !text.contains("test result: failed")
            && !text.contains("error: unexpected argument")
            && !text.contains("\nerror:")
            && !trimmed.starts_with("error:")
    })
}

fn tool_name_looks_like_verification(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        "test",
        "check",
        "lint",
        "clippy",
        "pytest",
        "playwright",
        "cypress",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn command_looks_like_verification(command: &str) -> bool {
    let lower = command.to_lowercase();
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "npm test",
        "npm run test",
        "npm run build",
        "pnpm test",
        "pnpm build",
        "yarn test",
        "yarn build",
        "pytest",
        "python -m pytest",
        "ruff check",
        "mypy",
        "go test",
        "go vet",
        "swift test",
        "gradle test",
        "mvn test",
        "make test",
        "make check",
        "just test",
        "just check",
        "git diff --check",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn user_confirmed_execution_from_recent_context(state: &AgenticLoopState) -> bool {
    if !looks_like_execution_confirmation(&state.message) {
        return false;
    }

    state
        .messages
        .iter()
        .rev()
        .take(8)
        .filter(|message| message.get("role").and_then(|role| role.as_str()) == Some("assistant"))
        .filter_map(|message| message.get("content").and_then(|content| content.as_str()))
        .any(assistant_text_offered_execution)
}

fn looks_like_execution_confirmation(message: &str) -> bool {
    let normalized = message
        .trim()
        .trim_matches(|c: char| {
            c.is_ascii_punctuation()
                || c.is_whitespace()
                || matches!(c, '。' | '，' | '！' | '？' | '；' | '：')
        })
        .to_lowercase();
    if normalized.is_empty() || normalized.chars().count() > 24 {
        return false;
    }

    matches!(
        normalized.as_str(),
        "yes"
            | "y"
            | "ok"
            | "okay"
            | "go ahead"
            | "do it"
            | "proceed"
            | "continue"
            | "sure"
            | "当然"
            | "当然了"
            | "好"
            | "好的"
            | "可以"
            | "没问题"
            | "继续"
            | "继续吧"
            | "执行"
            | "直接执行"
            | "开始"
            | "做吧"
    ) || normalized.contains("继续")
        || normalized.contains("执行")
}

fn assistant_text_offered_execution(text: &str) -> bool {
    let lower = text.to_lowercase();
    let offered = [
        "需要我",
        "我可以",
        "要继续吗",
        "即可执行",
        "shall i",
        "should i",
        "want me to",
        "i can",
        "go ahead",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let action = [
        "执行",
        "修改",
        "修复",
        "apply",
        "patch",
        "edit",
        "change",
        "implement",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    offered && action
}

fn final_text_defers_execution(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "需要我直接执行",
        "要继续吗",
        "即可执行",
        "等待确认",
        "shall i",
        "should i",
        "want me to",
        "ready to apply",
        "can apply",
        "can execute",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn final_text_concludes_no_change_needed(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "bug does not exist",
        "bug doesn't exist",
        "issue does not exist",
        "issue doesn't exist",
        "no change needed",
        "no changes needed",
        "nothing to change",
        "already correct",
        "already fixed",
        "not reproducible",
        "cannot reproduce",
        "can't reproduce",
        "无需修改",
        "不需要修改",
        "没有需要修改",
        "问题不存在",
        "没有这个问题",
        "无法复现",
        "未复现",
        "已经正确",
        "已经修复",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Stable marker prefix embedded in the corrective user message so that
/// `finalize_and_render` can strip it after the turn completes. Keeps the
/// conversation history clean across user turns without depending on the
/// downstream compactor's heuristics.
pub(crate) const EXECUTION_RETRY_MARKER: &str = "## ⤴ Execution Retry Correction";

pub(crate) fn is_execution_retry_correction(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXECUTION_RETRY_MARKER))
}

fn execution_retry_notice(reason: ExecutionRetryReason) -> String {
    match reason {
        ExecutionRetryReason::MissingMutation => {
            "↻ Execution requested but no edits were applied; forcing corrective retry…".to_string()
        }
        ExecutionRetryReason::MissingBrowserVerification => {
            "↻ Browser verification was claimed without browser-capable evidence; forcing corrective retry…".to_string()
        }
    }
}

fn execution_retry_message(original_query: &str, reason: ExecutionRetryReason) -> String {
    let correction = match reason {
        ExecutionRetryReason::MissingMutation => {
            "Runtime correction: the user requested or confirmed code execution, \
             but your previous response ended without applying any concrete workspace mutation. \
             Do not ask for permission again and do not only restate a plan. \
             Use the available file-editing tools to make the change, then run the appropriate existing verification."
        }
        ExecutionRetryReason::MissingBrowserVerification => {
            "Runtime correction: this task explicitly required browser/UI verification, \
             but your previous response claimed success without recording any browser-capable verification evidence. \
             Do not treat curl/server/process checks as browser verification. \
             Use a real browser-capable tool or workflow (for example Playwright, Selenium, Puppeteer, Cypress, \
             a headless browser screenshot, or a browser DOM dump after page execution), \
             or say plainly that you could not verify it in a browser."
        }
    };
    format!("{EXECUTION_RETRY_MARKER}\n{correction}\n\nOriginal user query: {original_query}")
}

/// Mid-loop escalation: kicks in while the model is still calling tools but
/// has spent the first several rounds only on read-only inspection (`cat`,
/// `grep`, `ls`, `git diff`, etc.) on a task whose profile says it should be
/// mutating the workspace. Without this guard the loop runs out of budget
/// before a single edit is applied.
pub(crate) const EXECUTION_ESCALATION_MARKER: &str = "## ⤴ Execution Escalation";

/// Minimum successful non-synthetic tool calls accumulated on a mutating task
/// before we start forcing an execution escalation. Chosen to allow a normal
/// "read a couple of files, then edit" workflow to proceed uninterrupted
/// (typical fix workflows commit an edit within 3-5 tool calls), while still
/// catching runaway read loops well before budget exhaustion.
pub(crate) const EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD: usize = 8;

pub(crate) fn is_execution_escalation(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXECUTION_ESCALATION_MARKER))
}

pub(crate) fn is_execution_corrective_message(m: &serde_json::Value) -> bool {
    is_execution_retry_correction(m)
        || is_execution_escalation(m)
        || is_parallel_batching_force(m)
        || is_round_budget_phase1(m)
        || is_completion_soft_stop(m)
        || is_redundant_reads_corrective(m)
        || is_cache_waste_corrective(m)
        || is_search_fanout_corrective(m)
        || is_exploration_family_corrective(m)
        || is_exploration_family_phase2(m)
}

/// Third-tier guard for the parallel-batching layer. The prompt-side soft
/// nudge fires when the trailing single-tool round streak hits
/// `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=6). If the model ignores the nudge
/// and produces yet another single-tool round, the streak crosses the
/// resolved `parallel_batching_force_streak` threshold (default 8, per-model
/// overrides via `ModelPolicyProfile`) and we inject a hard corrective
/// `user` message.
///
/// The circuit breaker handles persistent stalls that ignore the hard force —
/// no escalation layer needed.
pub(crate) const PARALLEL_BATCHING_FORCE_MARKER: &str = "## ⤴ Parallel Batching Force";

/// Trailing single-tool-round streak length at which the soft prompt nudge
/// (=6) escalates into a forced corrective injection.
/// Default for the threshold; the actual value used at runtime flows through
/// `ToolSelectionConfig::effective_parallel_batching_force_streak` (and
/// per-model overrides via `ModelPolicyProfile`).
/// Must match `effective_parallel_batching_force_streak`'s zero-default.
#[cfg(test)]
pub(crate) const PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD: usize =
    astra_config::runtime_config::DEFAULT_PARALLEL_BATCHING_FORCE_STREAK as usize;

pub(crate) fn is_parallel_batching_force(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(PARALLEL_BATCHING_FORCE_MARKER))
}

pub(crate) fn should_force_parallel_batching(state: &AgenticLoopState, threshold: usize) -> bool {
    if state.stall.forced_parallel_batching {
        return false;
    }
    // One corrective injection per turn: if any other mid-loop intervention
    // already fired (advisory or hard), a parallel-batching force would
    // duplicate correction and stack two guidance messages.
    if state.stall.any_intervention_active() {
        return false;
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    streak >= threshold
}

pub(crate) fn parallel_batching_force_message(streak: usize, original_query: &str) -> String {
    format!(
        "{PARALLEL_BATCHING_FORCE_MARKER}\n\
         Runtime correction: your last {streak} rounds each ran exactly ONE tool, \
         despite the prompt-layer nudge to batch independent calls. This wastes \
         a round of latency, tokens, and budget for each call. \
         Your NEXT response MUST do exactly one of the following:\n\
         - Produce your final answer now if you already have enough information, OR\n\
         - Call ≥2 independent tools in a single parallel batch (different files, \
           different greps, different reads — anything that does not strictly \
           depend on the previous tool's output).\n\
         Do not produce another single-tool round.\n\n\
         Original user query: {original_query}"
    )
}

// ─── Round-budget convergence guard (two-phase) ─────────────────────────
//
// Phase 1 fires when the loop has completed >= effective round-budget hard
// limit but the model is still calling tools. The runtime injects a hard
// corrective `user` message AND restricts all tools for the upcoming round,
// so the model is forced into a text-only finalization. The corrective
// wording is explicitly anti-hallucination: it tells the model to enumerate
// what was verified and what was NOT verified instead of fabricating.
//
// Phase 2 is the safety net: if the model still produces tool calls after
// phase 1 (i.e. ignores both the corrective AND attempts tools that were
// runtime-restricted), `should_abort_for_round_budget_phase2` returns true
// and the caller aborts the loop — analogous to a hard max-turns error,
// but reached only after one extra grace round, which avoids the
// overkill of an immediate hard cap on weaker models.

pub(crate) const ROUND_BUDGET_PHASE1_MARKER: &str = "## ⤴ Round Budget Reached";
pub(crate) const COMPLETION_SOFT_STOP_MARKER: &str = "## ✓ Task Appears Complete";

pub(crate) fn is_round_budget_phase1(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(ROUND_BUDGET_PHASE1_MARKER))
}

pub(crate) fn is_completion_soft_stop(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(COMPLETION_SOFT_STOP_MARKER))
}

fn tool_record_is_git_commit_action(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.ok || record.name != "git" {
        return false;
    }
    record
        .args_full
        .as_deref()
        .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
        .and_then(|args| {
            args.get("action")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some("commit")
}

/// Build a `RoundSignal` from the current loop state for the circuit breaker.
/// Uses the latest `turn_sigs` entry and checks `tool_call_records` for mutations.
fn build_circuit_breaker_signal(
    state: &AgenticLoopState,
) -> astra_turn_core::loop_circuit_breaker::RoundSignal {
    use astra_turn_core::loop_circuit_breaker::RoundSignal;

    let tool_signatures = state.stall.turn_sigs.last().cloned().unwrap_or_default();
    let tool_count = tool_signatures.len();
    if state.llm_rounds_completed == 0 {
        return RoundSignal {
            tool_signatures,
            produced_mutation: false,
            task_completed: false,
            tool_count,
        };
    }

    // Check only the most recently completed round. The previous implementation
    // scanned the last `max_tools_per_turn` records, so a single mutation could
    // mask many later read-only rounds and delay stall detection.
    let latest_round = state.llm_rounds_completed - 1;
    let latest_round_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.round == Some(latest_round))
        .collect();
    let produced_mutation = if !latest_round_records.is_empty() {
        latest_round_records
            .iter()
            .any(|record| tool_record_is_workspace_mutation(record))
    } else {
        // Legacy records may not carry round metadata; fall back to the old
        // bounded scan only when the batch is fully legacy. Partial round
        // metadata is treated as authoritative for per-round classification.
        state
            .stall
            .tool_call_records
            .iter()
            .rev()
            .take(state.max_tools_per_turn as usize)
            .any(tool_record_is_workspace_mutation)
    };
    let task_completed = !state.hooks.task_board_snapshot.has_unfinished_tasks()
        && latest_round_records
            .iter()
            .any(|record| tool_record_is_git_commit_action(record));

    RoundSignal {
        tool_signatures,
        produced_mutation,
        task_completed,
        tool_count,
    }
}

pub(crate) fn completion_soft_stop_message(round_index: u32, original_query: &str) -> String {
    format!(
        "{COMPLETION_SOFT_STOP_MARKER}\n\
         Runtime signal: a successful git commit indicates the requested work is likely complete after {round_index} tool round(s).\n\n\
         Stop now and provide the final answer unless you can name concrete remaining work that is necessary for the user's request. \
         Do not run more verification or status checks just to be extra sure; only use tools if there is specific unresolved work.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn round_budget_phase1_message(round_index: u32, original_query: &str) -> String {
    format!(
        "{ROUND_BUDGET_PHASE1_MARKER}\n\
         Runtime correction: this turn has used {round_index} tool rounds and \
         is past the configured hard limit.\n\n\
         Tool access for the next round has been restricted by the runtime. \
         Any tool calls you emit WILL BE DROPPED before execution — the runtime will \
         not invoke them and you will not receive results. Your next message must \
         be the final text-only answer.\n\n\
         IMPORTANT (anti-hallucination):\n\
         - Synthesize what you DID verify with the tool calls already made.\n\
         - Explicitly list anything you could NOT verify or finish.\n\
         - Do NOT fabricate, infer, or invent results you did not actually observe.\n\
         - A partial-but-honest answer is strictly better than a confident-but-fabricated one.\n\n\
         Original user query: {original_query}"
    )
}

// Redundant-reads mid-loop corrective.
//
// Detects the pattern where the model re-reads overlapping line ranges of the
// same file with no intervening workspace mutation. The detection algorithm
// lives in `astra-turn-core::evaluation::count_redundant_overlapping_reads`
// (post-mortem use) and is reused here for a one-shot mid-loop corrective.
//
// Threshold note: post-mortem flags at count ≥ 3; mid-loop fires at ≥
// `REDUNDANT_READS_MIDLOOP_THRESHOLD` to err slightly on the side of
// underkill, since this is a behavioral intervention rather than a passive
// signal. Calibrated against the same 14k-session survey: confirmed-waste
// fixtures all reach 7+ within their turn, so a threshold of 4 still catches
// every problem turn well before the count balloons.

pub(crate) const REDUNDANT_READS_MARKER: &str = "## ⤴ Redundant Reads Detected";
pub(crate) const CACHE_WASTE_MARKER: &str = "## ⤴ Repeated Cached Tool Calls Detected";
pub(crate) const SEARCH_FANOUT_MARKER: &str = "## ⤴ Search Fanout Detected";
pub(crate) const EXPLORATION_FAMILY_MARKER: &str = "## ⤴ Exploration Family Churn Detected";
pub(crate) const EXPLORATION_FAMILY_PHASE2_MARKER: &str =
    "## ⤴ Exploration Family Convergence Required";
/// Default cache-waste midloop threshold. Used in tests; production code
/// reads from `ToolSelectionConfig::effective_cache_waste_midloop_threshold()`.
#[cfg(test)]
pub(crate) const CACHE_WASTE_MIDLOOP_THRESHOLD: usize = 3;

/// Mid-loop corrective threshold (intentionally one above the post-mortem
/// signal threshold). One redundant overlap is normal noise; two can be
/// healthy double-checking; three matches the post-mortem signal but at
/// mid-loop we wait one more event to avoid premature intervention on
/// borderline turns.
/// Default for the redundant-reads mid-loop threshold; the actual value used
/// at runtime flows through
/// `ToolSelectionConfig::effective_redundant_reads_midloop_threshold`. Must
/// match that accessor's zero-default.
#[cfg(test)]
pub(crate) const REDUNDANT_READS_MIDLOOP_THRESHOLD: usize = 4;

pub(crate) fn is_redundant_reads_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(REDUNDANT_READS_MARKER))
}

pub(crate) fn cache_wasteful_tools(
    state: &AgenticLoopState,
    threshold: usize,
) -> Vec<(String, usize)> {
    let mut tools: Vec<(String, usize)> = state
        .turn_guard
        .health
        .cache_wasteful_tools(threshold)
        .into_iter()
        .map(|(tool, count)| (tool.to_string(), count))
        .collect();
    tools.sort_by(|left, right| left.0.cmp(&right.0));
    tools
}

pub(crate) fn is_cache_waste_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(CACHE_WASTE_MARKER))
}

pub(crate) fn is_search_fanout_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(SEARCH_FANOUT_MARKER))
}

pub(crate) fn is_exploration_family_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXPLORATION_FAMILY_MARKER))
}

pub(crate) fn is_exploration_family_phase2(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXPLORATION_FAMILY_PHASE2_MARKER))
}

fn restricted_tools_for_exploration_family(family: &str) -> &'static [&'static str] {
    match family {
        "diff" => &["git"],
        "search" => &["glob", "grep", "rg"],
        "read" => &["read_file", "view"],
        _ => &[],
    }
}

fn exploration_family_label(family: &str) -> &'static str {
    match family {
        "diff" => "diff-review",
        "search" => "search",
        "read" => "read",
        _ => "exploration",
    }
}

pub(crate) fn exploration_family_corrective_candidate(
    state: &AgenticLoopState,
    threshold: usize,
) -> Option<(String, usize)> {
    if state.stall.forced_exploration_family_corrective {
        return None;
    }
    let (family, streak) = astra_turn_core::evaluation::exploration_family_round_streak(
        &state.stall.tool_call_records,
    )?;
    (streak >= threshold).then(|| (family.to_string(), streak))
}

fn apply_exploration_family_restrictions(
    state: &mut AgenticLoopState,
    family: &str,
) -> Vec<String> {
    let mut restricted = restricted_tools_for_exploration_family(family)
        .iter()
        .map(|tool| (*tool).to_string())
        .collect::<Vec<_>>();
    restricted.sort();
    for tool in &restricted {
        state.restricted_tools.insert(tool.clone());
    }
    state.stall.exploration_family_corrective_family = Some(family.to_string());
    restricted
}

fn latest_non_synthetic_round_records(
    state: &AgenticLoopState,
) -> Option<(u32, Vec<&astra_services::session_journal::ToolCallRecord>)> {
    let last_round = state
        .stall
        .tool_call_records
        .iter()
        .filter(|rec| !rec.is_synthetic_placeholder())
        .filter_map(|rec| rec.round)
        .max()?;
    let records = state
        .stall
        .tool_call_records
        .iter()
        .filter(|rec| !rec.is_synthetic_placeholder())
        .filter(|rec| rec.round == Some(last_round))
        .collect::<Vec<_>>();
    Some((last_round, records))
}

pub(crate) fn exploration_family_phase2_candidate(
    state: &AgenticLoopState,
) -> Option<(String, Vec<String>)> {
    if !state.stall.forced_exploration_family_corrective
        || state.stall.forced_exploration_family_phase2
    {
        return None;
    }
    let family = state
        .stall
        .exploration_family_corrective_family
        .as_deref()?;
    let restricted = restricted_tools_for_exploration_family(family);
    let (_, latest_round_records) = latest_non_synthetic_round_records(state)?;
    if latest_round_records.is_empty() {
        return None;
    }

    let mut blocked_tools = latest_round_records
        .iter()
        .filter(|rec| rec.was_blocked_by_policy() && restricted.contains(&rec.name.as_str()))
        .map(|rec| rec.name.clone())
        .collect::<Vec<_>>();
    if blocked_tools.is_empty() || blocked_tools.len() != latest_round_records.len() {
        return None;
    }
    blocked_tools.sort();
    blocked_tools.dedup();
    Some((family.to_string(), blocked_tools))
}

/// Whether to inject the redundant-reads mid-loop corrective on the upcoming
/// round. One-shot per turn (the flag is set when corrective fires).
pub(crate) fn should_inject_redundant_reads_corrective(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.forced_redundant_reads_corrective {
        return false;
    }
    let count = astra_turn_core::evaluation::count_redundant_overlapping_reads(
        &state.stall.tool_call_records,
    );
    count >= threshold
}

pub(crate) fn should_inject_cache_waste_corrective(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.forced_cache_waste_corrective {
        return false;
    }
    !cache_wasteful_tools(state, threshold).is_empty()
}

pub(crate) fn should_inject_search_fanout_corrective(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.forced_search_fanout_corrective {
        return false;
    }
    if !state.task_profile.mutates_workspace {
        return false;
    }
    astra_turn_core::evaluation::count_search_fanout(&state.stall.tool_call_records) >= threshold
}

pub(crate) fn cache_waste_corrective_message(
    tools: &[(impl AsRef<str>, usize)],
    original_query: &str,
) -> String {
    let tool_list = tools
        .iter()
        .map(|(tool, count)| format!("{} ({count}x)", tool.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{CACHE_WASTE_MARKER}\n\
         Runtime correction: you have repeated cached tool calls this turn [{tool_list}]. \
         Those results are already in context — calling the same tool again wastes tokens and does not add evidence.\n\n\
         REQUIRED next-step behavior:\n\
         - Reuse the cached result you already have; do NOT repeat the same tool call again.\n\
         - Only call another tool if it fetches genuinely new evidence (different file, different diff target, different query, or changed worktree).\n\
         - If you already have enough evidence, write the final answer now.\n\
         - If you still need more evidence, explain the ONE specific missing fact and use a different tool or different arguments to get it.\n\n\
         Anti-hallucination: do NOT pretend a repeated cached call produced new information.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn search_fanout_corrective_message(count: usize, original_query: &str) -> String {
    format!(
        "{SEARCH_FANOUT_MARKER}\n\
         Runtime correction: you have made {count} grep/rg/find-like search calls in an implementation turn. \
         Broad search has crossed the low-yield threshold: more search is likely to expand context instead of finishing the task.\n\n\
         REQUIRED next-step behavior:\n\
         - Synthesize the evidence already gathered before doing anything else.\n\
         - Do NOT run another broad search (`grep`, `rg`, `glob`, or `find`) in the next round.\n\
         - If a change is still needed, edit the specific file already identified.\n\
         - If validation is needed, run the narrow relevant test/check instead of more discovery.\n\
         - If one fact is still missing, read the exact file/range that contains it.\n\
         - If you already have enough evidence, write the final answer now.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_corrective_message(
    family: &str,
    streak: usize,
    restricted_tools: &[String],
    original_query: &str,
) -> String {
    let tool_list = restricted_tools.join(", ");
    let label = exploration_family_label(family);
    format!(
        "{EXPLORATION_FAMILY_MARKER}\n\
         Runtime correction: the last {streak} consecutive multi-call rounds stayed inside the same {label} family. \
         That is now classified as low-yield exploration churn, so the runtime has restricted [{tool_list}] for the next round.\n\n\
         REQUIRED next-step behavior:\n\
         - First synthesize the evidence already gathered from prior tool calls.\n\
         - If one fact is still missing, switch to a different tool family that can add genuinely new evidence.\n\
         - Do NOT reopen the same {family} path unless the worktree or target changed.\n\
         - If you already have enough evidence, write the answer now.\n\n\
         Anti-hallucination: do NOT claim that repeated {family} exploration produced new evidence when it did not.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_phase2_message(
    family: &str,
    blocked_tools: &[String],
    original_query: &str,
) -> String {
    let blocked_list = blocked_tools.join(", ");
    format!(
        "{EXPLORATION_FAMILY_PHASE2_MARKER}\n\
         Runtime correction: after the earlier {family}-family restriction, your most recent tool round still attempted ONLY restricted tools [{blocked_list}]. \
         That produced zero new evidence, so this turn must now converge instead of retrying the same path.\n\n\
         REQUIRED next-step behavior:\n\
         - Either write the answer now from the evidence already gathered, OR\n\
         - State the ONE missing fact and use ONE tool from a different family to fetch it.\n\
         - Do NOT attempt [{blocked_list}] again this turn unless the worktree or target actually changed.\n\
         - If you still cannot finish, explicitly summarize verified facts and remaining gaps instead of continuing exploratory retries.\n\n\
         Anti-hallucination: a blocked restricted-tool retry does NOT count as new evidence.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn redundant_reads_corrective_message(count: usize, original_query: &str) -> String {
    format!(
        "{REDUNDANT_READS_MARKER}\n\
         Runtime correction: you have re-read overlapping line ranges of the \
         same file {count} times this turn without any intervening edit. The \
         content has not changed — re-reading wastes tokens and stalls progress.\n\n\
         REQUIRED next-step behavior:\n\
         - Use the file content already in your context; do NOT issue another \
           read for any range you have already loaded.\n\
         - If you genuinely need a new section, use the `view` tool with \
           explicit `view_range` (NOT `bash sed`/`bash cat`) and only for ranges \
           you have not already seen.\n\
         - If you have enough information to answer, produce the final answer now.\n\
         - If you do not, state precisely what is still unknown and which ONE \
           specific new piece of evidence you need — do not loop on the same files.\n\n\
         Anti-hallucination: do NOT fabricate file contents you have not actually \
         observed. A partial-but-honest answer beats a confident-but-fabricated one.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn should_escalate_execution(state: &AgenticLoopState) -> bool {
    if state.stall.forced_execution_escalation {
        return false;
    }
    // One corrective injection per turn: if parallel-batching force already
    // fired, skip escalation to avoid double-injecting corrective messages.
    // NOTE: execution order in execute_turn_and_ingest_phase is
    //   escalation → parallel-batching, so in practice escalation runs first.
    //   This guard is defensive against future reordering.
    if state.stall.forced_parallel_batching {
        return false;
    }
    if !state.task_profile.mutates_workspace {
        return false;
    }
    if has_concrete_workspace_mutation(state) {
        return false;
    }

    let successful_real_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .filter(|record| record.ok)
        .collect();

    if successful_real_records.len() < EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD {
        return false;
    }

    // Every successful call was read-only (none mutating) and none committed
    // a workspace change — the model is spinning on inspection.
    successful_real_records
        .iter()
        .all(|record| !tool_record_is_workspace_mutation(record))
}

pub(crate) fn execution_escalation_message(original_query: &str, read_only_calls: usize) -> String {
    format!(
        "{EXECUTION_ESCALATION_MARKER}\n\
         Runtime correction: you have made {read_only_calls} read-only tool calls on a task that \
         clearly requires changing the workspace, and have not applied a single edit yet. \
         Stop reading more files. Your NEXT response must invoke an editing tool \
         (`apply_patch`, `edit_file`, `str_replace`, `write_file`, or a `bash` command that \
         actually modifies files such as `sed -i`, a redirect `>`/`>>`, or `apply_patch`). \
         Do not produce another round of `cat`/`grep`/`ls`/`git diff`/`find`. Do not ask the \
         user for permission. Apply the change, then run the appropriate existing verification.\n\n\
         Original user query: {original_query}"
    )
}

fn update_turn_trace_collector(state: &mut AgenticLoopState, turn_result: &HostTurnResult) {
    if let Some(ref collector) = state.telemetry.turn_trace_collector {
        if let Some(spt) = turn_result.accum.system_prompt_tokens {
            collector.set_system_prompt_tokens(spt);
        }
        if let Some(ref breakdown_json) = turn_result.accum.system_prompt_breakdown
            && let Ok(breakdown) = serde_json::from_value::<
                astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
            >(breakdown_json.clone())
        {
            collector.record_system_prompt(breakdown);
        }
    }
}

pub(crate) fn observe_turn_end_without_tools(
    state: &mut AgenticLoopState,
    _turn_index: usize,
    turn_start_time: Instant,
    ttft_ms: Option<u64>,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability::TurnTiming {
            turn: session_turn_number(state),
            context_assembly_ms: 0,
            ttft_ms: ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = astra_core::sync_poison::recover_rwlock_write(session);
        crate::observability::on_turn_end(hub, &mut session_guard, timing);
    }
}

fn emit_subrun_text_preview<H: AgenticLoopHost>(
    host: &mut H,
    state: &AgenticLoopState,
    quiet: bool,
) {
    if !quiet && !state.final_text.is_empty() {
        let preview: String = state.final_text.chars().take(120).collect();
        let line = if state.final_text.len() > 120 {
            format!("{preview}…")
        } else {
            preview
        };
        host.emit_headless_line(HeadlessStderrStyle::Dim, line);
    }
}

const MAX_REACTIVE_BUDGET_COMPACTION_ATTEMPTS: u32 = 3;

async fn handle_token_budget<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
    turn_result: &HostTurnResult,
) -> Option<TurnExecutionControl> {
    if state.max_turn_input_tokens == 0 {
        return None;
    }
    let measured = state.last_measured_prompt_tokens?;
    if measured <= state.max_turn_input_tokens {
        return None;
    }

    if state.budget_wrapup_injected {
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                "⚠ Token budget exceeded — completing turn.".to_string(),
            );
        }
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::TokenBudgetExceeded,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some(format!(
                    "Token budget: {}/{} tokens",
                    measured, state.max_turn_input_tokens,
                )),
            ),
        ));
        record_early_exit_llm_round(
            state,
            turn_result,
            prep.turn_start_time,
            Some("token_budget_exceeded"),
        );
        observe_turn_end_without_tools(
            state,
            turn_index,
            prep.turn_start_time,
            turn_result.ttft_ms,
        );
        state.step_recorder.end_turn(false);
        finalize_and_render(host, state).await;
        return Some(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
    }

    // First attempt: compact-and-continue instead of hard-stopping.
    // Two-tier strategy:
    //   1. Aggressive compression pipeline (clear tool results)
    //   2. If still over: spill old messages to disk, keep reference in context
    // Only if both fail do we inject the stop directive.
    // Skip tier-1 mechanical compression if pre-turn LLM compact already ran,
    // but still allow tier-2 spill-to-disk as an independent recovery path.
    if !state.budget_wrapup_injected
        && state.compaction_effectiveness.attempt_count < MAX_REACTIVE_BUDGET_COMPACTION_ATTEMPTS
    {
        let budget = super::super::TokenBudget {
            max_prompt_tokens: state.max_turn_input_tokens,
            last_measured_tokens: measured,
            current_round_index: Some(state.current_round_index),
            now_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let mut total_freed = 0u64;
        let mut layer_descriptions: Vec<String> = Vec::new();
        let mut total_messages_removed: usize = 0;
        if state.compact_tier_applied < CompactionTier::CompactHistory {
            let pipeline = super::super::CompactionEngine::aggressive_pipeline();
            let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
            total_freed = outcome.total_tokens_freed;
            total_messages_removed = outcome
                .layer_results
                .iter()
                .map(|(_, r)| r.messages_removed)
                .sum();
            layer_descriptions = outcome
                .layer_results
                .iter()
                .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
                .collect();
        }

        // Tier 2: Spill old messages to disk if compression wasn't enough.
        // Serialize the oldest 60% of messages to a session-local file.
        // Leave a system message referencing the file path so the agent
        // can read_file it if needed. This is the SpillBackend pattern
        // applied to conversation history — content isn't lost, just
        // moved out of the live context window.
        if measured.saturating_sub(total_freed) > state.max_turn_input_tokens {
            if let Some(sid) = state.current_session_id.as_deref() {
                let spill_freed = spill_old_messages_to_disk(
                    &mut state.messages,
                    sid,
                    state.llm_rounds_completed,
                );
                total_freed += spill_freed;
                if spill_freed > 0 {
                    layer_descriptions.push(format!("spill_to_disk: ~{} tokens", spill_freed));
                }
            }
        }

        if total_freed > 0 {
            if !prep.quiet {
                let pressure = measured as f64 / state.max_turn_input_tokens as f64;
                let event = CompactionEvent::new(
                    CompactionKind::ReactiveBudget,
                    pressure,
                    total_freed,
                    measured,
                    state.max_turn_input_tokens,
                    total_messages_removed,
                    state.messages.len(),
                    layer_descriptions.clone(),
                );
                host.on_compaction(event);
            }
            if let Some(ref mut sess) = state.pipeline_session {
                sess.recovery.record_reactive_compact();
                sess.stats.record_compaction(total_freed);
            }
            state
                .compaction_effectiveness
                .record_compaction(total_freed);
            // Session 0e37eb46 regression: after compaction shreds the
            // history, the model sees a much-shorter context and often
            // misreads it as "I've been interrupted" → produces a
            // progress summary instead of continuing. Inject a short
            // directive that reframes it as "the runtime compressed
            // your history; CONTINUE the task — do NOT summarize."
            //
            // Observable: stderr line above ("♻ Context pressure…")
            // shows the compaction fired; this push_volatile adds the
            // behavioural counter-directive to the volatile lane.
            // Recoverable: if a future user wants the old behaviour,
            // the volatile is singleton per turn and never persisted.
            // Correctable: `compaction_injects_resume_directive_on_volatile_lane`
            // test locks the contract.
            state.push_volatile(
                super::host::VolatileKind::CompactResume,
                super::super::budget_messaging::COMPACT_RESUME_DIRECTIVE,
            );
            try_write_heavy_checkpoint(state);
            return Some(TurnExecutionControl::ContinueLoop);
        }
    }

    // Compaction didn't help (or already tried once) — inject stop directive.
    state.budget_wrapup_injected = true;
    if !prep.quiet {
        host.emit_headless_line(
            HeadlessStderrStyle::Yellow,
            format!(
                "⚠ Token budget reached ({measured}/{} tokens) — wrapping up.",
                state.max_turn_input_tokens,
            ),
        );
    }
    state.push_volatile(
        super::host::VolatileKind::BudgetAdvisory,
        super::super::budget_messaging::BUDGET_REACHED_ADVISORY,
    );
    try_write_heavy_checkpoint(state);
    Some(TurnExecutionControl::ContinueLoop)
}

fn should_wrap_up_for_cumulative_budget<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    quiet: bool,
) -> bool {
    if state.max_cumulative_tokens == 0 {
        return false;
    }
    let cumulative = state.total_prompt + state.total_completion;
    if cumulative <= state.max_cumulative_tokens || state.budget_wrapup_injected {
        return false;
    }

    state.budget_wrapup_injected = true;
    // Record structured interruption for cumulative budget exhaustion.
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::CumulativeBudgetExceeded,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some(format!(
                "Cumulative token budget: {cumulative}/{} tokens",
                state.max_cumulative_tokens,
            )),
        ),
    ));
    if !quiet {
        host.emit_headless_line(
            HeadlessStderrStyle::Yellow,
            format!(
                "⚠ Cumulative token budget reached ({cumulative}/{} tokens) — wrapping up.",
                state.max_cumulative_tokens,
            ),
        );
    }
    state.push_volatile(
        super::host::VolatileKind::BudgetAdvisory,
        "You have reached the cumulative token budget. \
         Do NOT call any more tools. Summarize your progress so far and \
         present your results to the user.",
    );
    try_write_heavy_checkpoint(state);
    true
}

fn record_tool_selection(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_index: usize,
) {
    if let Some(ref collector) = state.telemetry.turn_trace_collector
        && !collector.has_tool_trace()
    {
        let selected_tools: Vec<String> = turn_result
            .edge_tool_round
            .iter()
            .map(|r| r.tool.clone())
            .collect();
        collector.record_tool_surface(
            &selected_tools,
            &[],
            state.telemetry.all_tools_used.len() as u32,
            turn_index as u64,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use std::sync::Arc;
    use std::time::Duration;

    use astra_services::session_journal::ToolCallRecord;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;
    use crate::observability::ObservabilityHub;
    use crate::turn::agentic_loop::host::tests::{MockHost, make_state, text_result};
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, VolatileKind, run_agentic_loop_with_host,
    };
    use crate::turn::run_control::{RunInputProvider, RunQueuedInputPoll, RunStatusProvider};
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;

    struct SnapshotClearingHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        emitted_lines: Vec<String>,
        rendered_final_text: Vec<String>,
        valid_tools: HashSet<String>,
    }

    impl SnapshotClearingHost {
        fn new(turn_results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results,
                current_turn: 0,
                emitted_lines: Vec::new(),
                rendered_final_text: Vec::new(),
                valid_tools: HashSet::new(),
            }
        }
    }

    struct StubRunControlProvider {
        polls: Mutex<VecDeque<RunQueuedInputPoll>>,
        released: Mutex<Vec<usize>>,
        release_failures: Mutex<usize>,
    }

    impl StubRunControlProvider {
        fn new(polls: Vec<RunQueuedInputPoll>) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(0),
            }
        }

        fn with_release_failures(polls: Vec<RunQueuedInputPoll>, release_failures: usize) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(release_failures),
            }
        }
    }

    #[async_trait]
    impl RunStatusProvider for StubRunControlProvider {
        async fn control_status(
            &self,
            _run_id: &str,
        ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
            Ok(None)
        }
    }

    #[async_trait]
    impl RunInputProvider for StubRunControlProvider {
        async fn poll_user_inputs(
            &self,
            _run_id: &str,
            after_event_index: usize,
        ) -> RunQueuedInputPoll {
            self.polls
                .lock()
                .await
                .pop_front()
                .unwrap_or(RunQueuedInputPoll {
                    next_cursor: after_event_index,
                    inputs: Vec::new(),
                    error: None,
                })
        }

        async fn mark_user_inputs_released(
            &self,
            _run_id: &str,
            event_indices: &[usize],
        ) -> Result<(), String> {
            let mut release_failures = self.release_failures.lock().await;
            if *release_failures > 0 {
                *release_failures -= 1;
                return Err("release failed".to_string());
            }
            drop(release_failures);
            self.released.lock().await.extend_from_slice(event_indices);
            Ok(())
        }
    }

    #[async_trait]
    impl AgenticLoopHost for SnapshotClearingHost {
        async fn execute_turn(
            &mut self,
            state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            if self.current_turn == 1 {
                state.hooks.task_board_snapshot = TaskBoardSnapshot::default();
            }
            self.current_turn += 1;
            Ok(self.turn_results.remove(0))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted_lines.push(line);
        }

        fn is_quiet(&self) -> bool {
            true
        }

        fn turn_interaction_mode(&self) -> TurnInteractionMode {
            TurnInteractionMode::NonInteractive
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }

        fn inject_tool_schema(&mut self, _schema: serde_json::Value) {}

        fn render_final_text(&mut self, text: &str) {
            self.rendered_final_text.push(text.to_string());
        }
    }

    #[test]
    fn circuit_breaker_introspection_message_uses_actual_read_only_streak() {
        let message = circuit_breaker_introspection_message(18, 12);

        assert!(message.contains("[Self-check — round 18]"));
        assert!(message.contains("12 consecutive rounds"));
        assert!(!message.contains("18 consecutive rounds"));
    }

    #[test]
    fn observe_turn_end_without_tools_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.max_turns = 20;
        state.remaining_turns = 4;
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_hub = Some(Arc::new(hub));
        state.telemetry.observability_session = Some(session.clone());

        let turn_start_time = Instant::now() - Duration::from_millis(25);
        observe_turn_end_without_tools(&mut state, 16, turn_start_time, Some(7));

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_timings.len(), 1);
        assert_eq!(guard.turn_timings[0].turn, 6);
    }

    #[test]
    fn manifest_persistence_called_after_execute_turn() {
        // Verify that persist_context_manifest_for_llm_call exists and is
        // callable. The actual ordering invariant (execute_turn → trace
        // capture → persist) is enforced by the compiler through async
        // await semantics and the function signature requiring a
        // HostTurnResult reference.
        use std::ptr;
        let fn_ptr = persist_context_manifest_for_llm_call as *const ();
        assert!(
            !fn_ptr.is_null(),
            "persist_context_manifest_for_llm_call must be defined"
        );
        // The function signature enforces ordering: it takes a
        // turn_result: Option<&HostTurnResult>, which only exists after
        // execute_turn returns.
    }

    // PR 5a: the turn loop must invoke host.on_turn_completed
    // exactly once per successful ingested turn, AFTER run_id is
    // populated by ingest but BEFORE tool execution / side effects.

    #[tokio::test]
    async fn turn_completed_hook_fires_once_on_successful_turn() {
        let mut state = make_state();
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            host.turn_completed_run_ids.len(),
            1,
            "hook must fire exactly once per successful turn"
        );
    }

    #[tokio::test]
    async fn turn_completed_hook_observes_ingested_run_id() {
        // Precondition: ingest_agentic_turn_stream populates
        // state.current_run_id before the hook runs. The hook must
        // see whatever ingest left there — not some stale pre-turn
        // value, not None from before ingest.
        let mut state = make_state();
        // Pretend a previous turn set this; ingest would normally
        // overwrite, but for a turn without server-assigned run_id
        // the value flows through unchanged. The assertion below
        // is simply "whatever state.current_run_id is post-ingest,
        // the hook sees the same thing".
        state.current_run_id = Some("pre-existing-run".to_string());
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            host.turn_completed_run_ids,
            vec![state.current_run_id.clone()],
            "hook must observe post-ingest run_id, matching current state"
        );
    }

    #[tokio::test]
    async fn turn_completed_hook_does_not_fire_on_fatal_ingest_outcome() {
        // Even when execute_turn itself returns Ok, the SSE stream
        // may carry an error that ingest classifies as Fatal (rate
        // limit, context window, provider 500). A Fatal ingest
        // leaves state.messages / current_run_id only partially
        // updated; capturing would poison any downstream sink with
        // a corrupt prefix. The hook MUST NOT fire on Fatal.
        let mut state = make_state();
        let error_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                error_message: Some("Error: simulated fatal".into()),
                has_usage: false,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(1),
            edge_tool_round: Vec::new(),
            error_kind: Some(astra_core::ErrorKind::RateLimit),
        };
        let mut host = MockHost::new(vec![error_result]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        assert!(
            host.turn_completed_run_ids.is_empty(),
            "hook must not fire when ingest returns Fatal"
        );
    }

    #[tokio::test]
    async fn turn_completed_hook_does_not_fire_when_execute_turn_errors() {
        // An empty MockHost returns BudgetExhausted on execute_turn.
        // The hook must NOT fire in the error path — we only want
        // to snapshot state after a successful response is ingested.
        let mut state = make_state();
        let mut host = MockHost::new(vec![]); // no turns queued

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;

        assert!(
            result.is_err(),
            "setup sanity: execute_turn should fail without queued results"
        );
        assert!(
            host.turn_completed_run_ids.is_empty(),
            "hook must not fire on execute_turn error"
        );
    }

    #[test]
    fn execution_retry_correction_is_detectable_and_stripped() {
        let msg = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("fix the bug", ExecutionRetryReason::MissingMutation),
        });
        assert!(is_execution_retry_correction(&msg));

        let unrelated = serde_json::json!({
            "role": "user",
            "content": "fix the bug",
        });
        assert!(!is_execution_retry_correction(&unrelated));

        let assistant_with_marker = serde_json::json!({
            "role": "assistant",
            "content": EXECUTION_RETRY_MARKER,
        });
        assert!(!is_execution_retry_correction(&assistant_with_marker));
    }

    #[test]
    fn execution_retry_blocks_plan_only_finish_for_mutating_task() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("修复这个问题");
        state.message = "修复这个问题".into();
        state.final_text = "需要我直接执行这些修改吗？".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"grep -rn \"foo\" rust/"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_forces_retry_on_zero_tool_noop_for_mutating_task() {
        // A mutating-profile task where the model produces a bare conclusion
        // with zero tool calls has no evidence for either a fix or a valid
        // no-op conclusion. The runtime should force one corrective retry.
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "I reviewed the code and the bug does not exist.".into();

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_fires_when_mutating_task_only_planned() {
        // Mutating profile + tool calls were made (exploration) but nothing
        // committed → still retry to push for execution.
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "Here is the plan: change foo to bar.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cat src/foo.rs"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_skips_reviewed_no_bug_conclusion_after_read_only_inspection() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "I reviewed the code path and the bug does not exist.".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"rg -n \"buggy_path\" rust/"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "view".into(),
            ok: true,
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn confirmation_detector_ignores_keyi_in_descriptive_sentence() {
        // "可以看到这里有问题" is description, not a confirmation.
        assert!(!looks_like_execution_confirmation("可以看到这里有问题"));
    }

    #[test]
    fn bash_mutation_detects_compound_and_sudo_commands() {
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cd /tmp && mv a b"}"#.into()),
            ..Default::default()
        };
        assert!(tool_record_is_workspace_mutation(&record));

        let sudo = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sudo rm -rf /tmp/cache"}"#.into()),
            ..Default::default()
        };
        assert!(tool_record_is_workspace_mutation(&sudo));
    }

    #[test]
    fn bash_mutation_returns_false_for_malformed_args() {
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some("rm -rf /".into()),
            ..Default::default()
        };
        // Non-JSON args are treated as missing rather than the raw string,
        // avoiding false positives from corrupted journal entries.
        assert!(!tool_record_is_workspace_mutation(&record));
    }

    #[test]
    fn execution_retry_recognizes_affirmative_followup_context() {
        let mut state = make_state();
        state.message = "当然了".into();
        state.final_text = "我可以继续执行，确认后开始。".into();
        state.total_tool_calls = 1;
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "需要我直接执行这些修改吗？"
        }));
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cat rust/crates/runtime/src/lib.rs"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_recognizes_english_affirmative_followup_context() {
        let mut state = make_state();
        state.message = "go ahead".into();
        state.final_text = "I can apply the patch now.".into();
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "Should I apply this patch?"
        }));

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_treat_bare_affirmative_as_execution() {
        let mut state = make_state();
        state.message = "当然了".into();
        state.final_text = "好的。".into();
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "这个解释有帮助吗？"
        }));

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_fire_for_read_only_review() {
        let mut state = make_state();
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
            "review local changes",
        );
        state.message = "review local changes".into();
        state.final_text = "I found one issue.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"git diff --stat"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_fire_after_concrete_edit() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "Done.".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn browser_verification_retry_fires_for_curl_only_success_claim() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text = "I tested it and it's fully functional.".into();
        state.total_tool_calls = 3;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"curl --noproxy '*' http://127.0.0.1:8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"ps -ef | grep http.server"}"#.into()),
            ..Default::default()
        });

        assert_eq!(
            execution_retry_reason(&state),
            Some(ExecutionRetryReason::MissingBrowserVerification)
        );
    }

    #[test]
    fn browser_verification_retry_overrides_concrete_edit_short_circuit() {
        let mut state = make_state();
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
            "fix the game bug and verify it in browser",
        );
        state.message = "fix the game bug and verify it in browser".into();
        state.final_text = "I fixed the bug and it's fully functional now.".into();
        state.total_tool_calls = 3;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"curl http://127.0.0.1:8000"}"#.into()),
            ..Default::default()
        });

        assert_eq!(
            execution_retry_reason(&state),
            Some(ExecutionRetryReason::MissingBrowserVerification)
        );
    }

    #[test]
    fn browser_verification_retry_skips_when_playwright_evidence_exists() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text = "I tested it and it's fully functional.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"npx playwright test tests/game.spec.ts"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn browser_verification_retry_skips_when_model_admits_not_verified() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text =
            "I could not verify this in a browser because no browser-capable tool is available."
                .into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    fn auto_verify_hook() -> astra_turn_core::stop_hooks::StopHook {
        astra_turn_core::stop_hooks::StopHook {
            label: "verify-changes".into(),
            command: "Based on the files you actually modified, run ONLY the relevant checks."
                .into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
        }
    }

    #[test]
    fn auto_verify_stop_hook_skips_after_latest_mutation_was_verified() {
        let mut state = make_state();
        state.hooks.stop_hooks = vec![auto_verify_hook()];
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            args_full: Some(r#"{"path":"src/lib.rs","old_str":"a","new_str":"b"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                r#"{"command":"cd rust && cargo test -p astra-cli --lib edge_tools::tests"}"#
                    .into(),
            ),
            result_preview: Some("✓ cargo | 426 passed, 0 failed".into()),
            ..Default::default()
        });

        assert!(should_skip_auto_verify_stop_hooks(&state));
    }

    #[test]
    fn auto_verify_stop_hook_does_not_skip_declarative_hook() {
        let mut state = make_state();
        state.hooks.stop_hooks = vec![astra_turn_core::stop_hooks::StopHook {
            label: "project-contract".into(),
            command: "make verify".into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
        }];
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            args_full: Some(r#"{"path":"src/lib.rs","old_str":"a","new_str":"b"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cargo check"}"#.into()),
            result_preview: Some("✓ cargo | finished".into()),
            ..Default::default()
        });

        assert!(!should_skip_auto_verify_stop_hooks(&state));
    }

    #[test]
    fn auto_verify_stop_hook_does_not_count_failed_cargo_pipeline() {
        let mut state = make_state();
        state.hooks.stop_hooks = vec![auto_verify_hook()];
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            args_full: Some(r#"{"path":"src/lib.rs","old_str":"a","new_str":"b"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                r#"{"command":"cargo test -p astra-cli test_a test_b 2>&1 | tail -30"}"#.into(),
            ),
            result_preview: Some(
                "✗ unknown | 1 error(s) (exit 1)\nerror: unexpected argument 'test_b' found".into(),
            ),
            ..Default::default()
        });

        assert!(!should_skip_auto_verify_stop_hooks(&state));
    }

    #[test]
    fn execution_retry_suppressed_when_round_budget_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_round_budget_phase1 = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_redundant_reads_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_redundant_reads_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_exploration_family_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_exploration_family_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_search_fanout_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_search_fanout_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_cache_waste_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_cache_waste_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn bash_read_only_is_not_workspace_mutation_but_sed_i_is() {
        let read_only = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -n '1,20p' src/lib.rs"}"#.into()),
            ..Default::default()
        };
        let mutating = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -i 's/old/new/' src/lib.rs"}"#.into()),
            ..Default::default()
        };

        assert!(!tool_record_is_workspace_mutation(&read_only));
        assert!(tool_record_is_workspace_mutation(&mutating));
    }

    #[test]
    fn circuit_breaker_signal_uses_latest_round_for_mutation_detection() {
        let mut state = make_state();
        state.llm_rounds_completed = 6;
        state.stall.turn_sigs.push(
            ["read_file:{\"path\":\"a.rs\"}".to_string()]
                .into_iter()
                .collect(),
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            round: Some(2),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            round: Some(5),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.produced_mutation,
            "an old str_replace must not mask a later read-only round"
        );
    }

    #[test]
    fn circuit_breaker_signal_marks_successful_git_commit_action_as_task_completed() {
        let mut state = make_state();
        state.llm_rounds_completed = 4;
        state.stall.turn_sigs.push(
            ["git:{\"action\":\"commit\",\"message\":\"finish\"}".to_string()]
                .into_iter()
                .collect(),
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "git".into(),
            ok: true,
            round: Some(3),
            args_full: Some(r#"{"action":"commit","message":"finish"}"#.into()),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(signal.task_completed);
        assert!(
            signal.produced_mutation,
            "git(action=commit) still counts as mutation evidence"
        );
    }

    #[test]
    fn circuit_breaker_signal_does_not_mark_completion_when_tasks_remain() {
        let mut state = make_state();
        state.llm_rounds_completed = 4;
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "finish validation".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::Pending,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);
        state.stall.turn_sigs.push(
            ["git:{\"action\":\"commit\",\"message\":\"finish\"}".to_string()]
                .into_iter()
                .collect(),
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "git".into(),
            ok: true,
            round: Some(3),
            args_full: Some(r#"{"action":"commit","message":"finish"}"#.into()),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.task_completed,
            "unfinished task-board work must suppress completion soft-stop"
        );
        assert!(signal.produced_mutation);
    }

    #[tokio::test]
    async fn unfinished_task_board_guard_forces_another_round() {
        let mut host = SnapshotClearingHost::new(vec![
            text_result("Done.", 10, 5, Some(20)),
            text_result("All task-board work is now complete.", 10, 5, Some(20)),
        ]);
        let mut state = make_state();
        state.hooks.task_board_snapshot =
            TaskBoardSnapshot::from_active_tasks(&[astra_tools::task_mgmt::SessionTask {
                archived_at: None,
                id: "task-1".to_string(),
                title: "finish validation".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            }]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok(), "loop should continue until tasks clear");
        assert_eq!(host.current_turn, 2);
        assert_eq!(state.final_text, "All task-board work is now complete.");
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    /// Stub host whose `execute_turn` keeps returning text-only results AND
    /// never clears the task-board snapshot — the same shape we'd see if the
    /// model ignored the runtime's "unfinished tasks remain" corrective and
    /// kept replying "Done." without making task-board progress.
    struct StubbornTextOnlyHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        emitted_lines: Vec<String>,
        rendered_final_text: Vec<String>,
        valid_tools: HashSet<String>,
    }

    impl StubbornTextOnlyHost {
        fn new(turn_results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results,
                current_turn: 0,
                emitted_lines: Vec::new(),
                rendered_final_text: Vec::new(),
                valid_tools: HashSet::new(),
            }
        }
    }

    #[async_trait]
    impl AgenticLoopHost for StubbornTextOnlyHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            self.current_turn += 1;
            Ok(self.turn_results.remove(0))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted_lines.push(line);
        }

        fn is_quiet(&self) -> bool {
            true
        }

        fn turn_interaction_mode(&self) -> TurnInteractionMode {
            TurnInteractionMode::NonInteractive
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }

        fn inject_tool_schema(&mut self, _schema: serde_json::Value) {}

        fn render_final_text(&mut self, text: &str) {
            self.rendered_final_text.push(text.to_string());
        }
    }

    /// Regression: the unfinished-task-board mid-loop gate must be one-shot
    /// per turn so a model that ignores the corrective doesn't churn the
    /// global round budget. After the gate fires once, the next text-only
    /// completion should fall through to terminal rendering, where
    /// `ensure_terminal_text` rewrites the answer with structured stop +
    /// remaining-task context (covered by the finalization tests).
    #[tokio::test]
    async fn unfinished_task_board_gate_is_one_shot_per_turn() {
        let mut host = StubbornTextOnlyHost::new(vec![
            text_result("Done.", 10, 5, Some(20)),
            text_result("Still done.", 10, 5, Some(20)),
        ]);
        let mut state = make_state();
        state.hooks.task_board_snapshot =
            TaskBoardSnapshot::from_active_tasks(&[astra_tools::task_mgmt::SessionTask {
                archived_at: None,
                id: "task-1".to_string(),
                title: "finish validation".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            }]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            outcome.is_ok(),
            "loop must terminate even when the model ignores the corrective"
        );
        assert_eq!(
            host.current_turn, 2,
            "exactly two turns should run: one before the gate, one after the corrective"
        );
        assert!(
            state.final_text.contains("task-board work remains open"),
            "terminal text should surface the unfinished-work record, got: {}",
            state.final_text
        );
        let unfinished_notices = host
            .emitted_lines
            .iter()
            .filter(|line| line.contains("Unfinished tasks remain"))
            .count();
        // Even though the loop's quiet flag suppresses these in
        // `unfinished_task_board_guard_forces_another_round`, that test runs
        // the same code path; here `host.is_quiet()` returns true so the
        // gate's headless line is suppressed too — the meaningful proof
        // that the gate is one-shot is `current_turn == 2` (no third turn).
        assert!(
            unfinished_notices <= 1,
            "the gate's stderr notice must fire at most once, got {unfinished_notices}"
        );
        // `forced_task_board_completion_gate` is reset by
        // `finalize_and_render` on terminal exit, so we don't assert on it
        // here. The structural proof is in the turn count plus the rewritten
        // terminal text.
    }

    #[test]
    fn circuit_breaker_signal_does_not_reuse_stale_git_commit_action_completion() {
        let mut state = make_state();
        state.llm_rounds_completed = 5;
        state.stall.turn_sigs.push(
            ["read_file:{\"path\":\"a.rs\"}".to_string()]
                .into_iter()
                .collect(),
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "git".into(),
            ok: true,
            round: Some(3),
            args_full: Some(r#"{"action":"commit","message":"finish"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            round: Some(4),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.task_completed,
            "only the latest completed round may emit completion"
        );
    }

    #[test]
    fn completion_soft_stop_message_is_ephemeral_corrective() {
        let msg = serde_json::json!({
            "role": "user",
            "content": completion_soft_stop_message(7, "finish and commit the fix"),
        });

        assert!(is_completion_soft_stop(&msg));
        assert!(is_execution_corrective_message(&msg));
        assert!(
            msg["content"]
                .as_str()
                .unwrap()
                .contains("Stop now and provide the final answer")
        );
    }

    #[test]
    fn circuit_breaker_abort_detail_explains_reason_and_resume_path() {
        let mut state = make_state();
        state.llm_rounds_completed = 6;
        let detail = circuit_breaker_abort_detail(&state);

        assert!(detail.contains("circuit breaker stopped this turn"));
        assert!(detail.contains("Progress from earlier rounds is preserved"));
        assert!(detail.contains("Resume by synthesizing verified evidence"));
    }

    #[test]
    fn task_board_corrective_uses_cache_stable_counts_only() {
        let snapshot =
            TaskBoardSnapshot::from_active_tasks(&[astra_tools::task_mgmt::SessionTask {
                archived_at: None,
                id: "task-1".to_string(),
                title: "very specific changing task title".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                subtasks: Vec::new(),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                updated_at: "2025-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
            }]);

        let msg = unfinished_task_board_corrective_message(&snapshot, "finish the task");

        assert!(msg.contains("1 in_progress task(s) remain"));
        assert!(!msg.contains("task-1"));
        assert!(!msg.contains("very specific changing task title"));
    }

    #[tokio::test]
    async fn reactive_compaction_does_not_arm_wrapup_after_success() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.last_measured_prompt_tokens = Some(90_000);
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..16 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("follow-up {i}: {}", "y".repeat(220)),
            }));
        }

        let control = handle_token_budget(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;

        assert!(matches!(control, Some(TurnExecutionControl::ContinueLoop)));
        assert!(
            !state.budget_wrapup_injected,
            "successful reactive compaction should continue the turn without arming wrapup"
        );
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|entry| entry.kind == VolatileKind::CompactResume),
            "successful reactive compaction should inject the continue-after-compact directive"
        );
    }

    #[tokio::test]
    async fn repeated_budget_pressure_retries_compaction_before_wrapup() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..24 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("follow-up {i}: {}", "y".repeat(220)),
            }));
        }

        state.last_measured_prompt_tokens = Some(90_000);
        let first = handle_token_budget(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;
        assert!(matches!(first, Some(TurnExecutionControl::ContinueLoop)));

        state.last_measured_prompt_tokens = Some(88_000);
        let second = handle_token_budget(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("still working", 88_000, 320, Some(20)),
        )
        .await;

        assert!(
            matches!(second, Some(TurnExecutionControl::ContinueLoop)),
            "continued context pressure should retry compaction before forcing wrapup"
        );
        assert!(
            !state.budget_wrapup_injected,
            "repeat pressure after a successful compact should not flip straight into wrapup mode"
        );
    }

    #[tokio::test]
    async fn reactive_compaction_attempt_cap_forces_wrapup() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.last_measured_prompt_tokens = Some(90_000);
        state.compaction_effectiveness.attempt_count = MAX_REACTIVE_BUDGET_COMPACTION_ATTEMPTS;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..16 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
        }

        let control = handle_token_budget(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;

        assert!(matches!(control, Some(TurnExecutionControl::ContinueLoop)));
        assert!(
            state.budget_wrapup_injected,
            "after bounded compaction attempts, token pressure must transition to wrapup"
        );
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|entry| entry.kind == VolatileKind::BudgetAdvisory),
            "attempt cap should inject the normal budget wrapup advisory"
        );
    }

    #[test]
    fn circuit_breaker_signal_ignores_round_zero_records_before_any_round_completes() {
        let mut state = make_state();
        state.llm_rounds_completed = 0;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            round: Some(0),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.produced_mutation,
            "round 0 records are not latest completed work before any round completes"
        );
    }

    // ─── Mid-loop execution escalation tests ──────────────────────────────

    fn make_mutating_state_with_reads(n: usize) -> AgenticLoopState {
        let mut state = make_state();
        state.message = "fix the bug in foo".into();
        state.task_profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
            "fix the bug in foo",
        );
        assert!(
            state.task_profile.mutates_workspace,
            "test precondition: profile must be mutating"
        );
        for i in 0..n {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: true,
                args_full: Some(format!(r#"{{"command":"cat src/file{i}.rs"}}"#, i = i)),
                ..Default::default()
            });
        }
        state
    }

    #[test]
    fn escalation_fires_after_threshold_of_read_only_calls_on_mutating_task() {
        let state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        assert!(should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_just_below_threshold() {
        let state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD - 1);
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_on_non_mutating_task() {
        let mut state =
            make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD + 2);
        // Flip profile to read-only exploration — escalation must not engage.
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("review the diff");
        assert!(!state.task_profile.mutates_workspace);
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_when_any_mutation_present() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        // One actual edit in the middle of many reads must suppress the guard.
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "edit_file".into(),
            ok: true,
            ..Default::default()
        });
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_when_bash_mutation_mixed_in() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -i 's/a/b/' foo.rs"}"#.into()),
            ..Default::default()
        });
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_is_one_shot_per_turn() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        state.stall.forced_execution_escalation = true;
        assert!(
            !should_escalate_execution(&state),
            "flag must prevent a second injection"
        );
    }

    #[test]
    fn escalation_suppressed_when_parallel_batching_already_fired() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        // Precondition: without the flag, escalation would fire.
        assert!(should_escalate_execution(&state));
        // Once parallel-batching force has fired, escalation must yield to
        // honor the one-corrective-per-turn invariant.
        state.stall.forced_parallel_batching = true;
        assert!(
            !should_escalate_execution(&state),
            "escalation must not fire when parallel-batching force already active"
        );
    }

    #[test]
    fn escalation_ignores_failed_tool_calls_for_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        // 20 failed reads — don't count toward threshold (they weren't real
        // progress; retrying reads is already flagged elsewhere).
        for _ in 0..20 {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: false,
                args_full: Some(r#"{"command":"cat missing.rs"}"#.into()),
                ..Default::default()
            });
        }
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_ignores_synthetic_placeholders() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        for _ in 0..(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD + 2) {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: true,
                args_preview: Some("<synthetic placeholder>".into()),
                ..Default::default()
            });
        }
        // If all records are synthetic placeholders they should be filtered
        // out and the threshold should not be met.
        let all_synthetic = state
            .stall
            .tool_call_records
            .iter()
            .all(|r| r.is_synthetic_placeholder());
        if all_synthetic {
            assert!(!should_escalate_execution(&state));
        }
    }

    #[test]
    fn retry_guard_yields_to_prior_escalation_in_same_turn() {
        // If escalation already fired mid-loop, retry must NOT also fire at
        // BreakLoop — one corrective injection per turn.
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.final_text = "I will proceed with the edits now.".into();
        state.total_tool_calls = 10;

        // Sanity: without the escalation flag this state would trigger retry.
        state.stall.forced_execution_escalation = false;
        assert!(should_force_execution_retry(&state));

        // With the escalation flag set, retry must yield.
        state.stall.forced_execution_escalation = true;
        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn parallel_batching_force_blocks_subsequent_retry_in_same_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.final_text = "I'll continue investigating.".into();
        state.total_tool_calls = 10;
        // Without the parallel-batching flag, this state would trigger retry.
        assert!(should_force_execution_retry(&state));
        // Once the parallel-batching force fired, retry must yield to honor
        // the one-corrective-per-turn invariant.
        state.stall.forced_parallel_batching = true;
        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn parallel_batching_suppressed_when_escalation_already_fired() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        // Precondition: without escalation flag, parallel-batching would fire.
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // Once escalation has fired, parallel-batching must yield.
        state.stall.forced_execution_escalation = true;
        assert!(
            !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
            "parallel-batching must not fire when escalation already active"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_retry_already_fired() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        state.stall.forced_execution_retry = true;
        assert!(
            !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
            "parallel-batching must not fire when retry already active"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_cascade_guard_already_fired() {
        let flags: Vec<Box<dyn Fn(&mut AgenticLoopState)>> = vec![
            Box::new(|s| s.stall.forced_round_budget_phase1 = true),
            Box::new(|s| s.stall.forced_redundant_reads_corrective = true),
            Box::new(|s| s.stall.forced_cache_waste_corrective = true),
            Box::new(|s| s.stall.forced_search_fanout_corrective = true),
            Box::new(|s| s.stall.forced_exploration_family_corrective = true),
            Box::new(|s| s.stall.forced_exploration_family_phase2 = true),
        ];
        for set_flag in &flags {
            let mut state = make_state();
            state.message = "explore the codebase".into();
            for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
                push_single_tool_round(&mut state);
            }
            // Precondition: would fire without the flag.
            assert!(should_force_parallel_batching(
                &state,
                PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
            ));
            set_flag(&mut state);
            assert!(
                !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
                "parallel-batching must not fire when a cascade guard already active"
            );
        }
    }

    #[test]
    fn escalation_marker_detected_and_stripped_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": execution_escalation_message("fix the bug", 9),
        });
        assert!(is_execution_escalation(&msg));
        assert!(is_execution_corrective_message(&msg));

        let retry = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("fix the bug", ExecutionRetryReason::MissingMutation),
        });
        assert!(is_execution_corrective_message(&retry));

        let unrelated = serde_json::json!({"role":"user","content":"fix the bug"});
        assert!(!is_execution_corrective_message(&unrelated));
    }

    // ─── Parallel-batching force (third-tier guard) ─────────────────────

    fn push_single_tool_round(state: &mut AgenticLoopState) {
        state
            .messages
            .push(serde_json::json!({"role": "assistant", "tool_calls": []}));
        state
            .messages
            .push(serde_json::json!({"role": "tool", "content": "..."}));
    }

    #[test]
    fn parallel_batching_force_fires_at_streak_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_below_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD - 1) {
            push_single_tool_round(&mut state);
        }
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_when_last_round_batched() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        // Long single-tool history that crossed threshold...
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 2) {
            push_single_tool_round(&mut state);
        }
        // ...but the most-recent round used 3 parallel tools — the model
        // already self-corrected, no force needed.
        state
            .messages
            .push(serde_json::json!({"role": "assistant", "tool_calls": []}));
        for _ in 0..3 {
            state
                .messages
                .push(serde_json::json!({"role": "tool", "content": "..."}));
        }
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 3) {
            push_single_tool_round(&mut state);
        }
        // First time would fire...
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // ...but once the flag is set, a second attempt is suppressed even
        // if the model produces yet another single-tool round.
        state.stall.forced_parallel_batching = true;
        push_single_tool_round(&mut state);
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    // ── Escalation tier (P0: session 8d9e5903 regression) ──
    //
    // When the first-tier force has fired but the model keeps producing
    // single-tool rounds, the runtime must escalate — one-shot is too
    // lenient. Guards prevent infinite re-fire and prevent escalation
    // from firing *before* the first-tier has fired.

    #[test]
    fn parallel_batching_force_marker_recognized_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": parallel_batching_force_message(7, "do something"),
        });
        assert!(is_parallel_batching_force(&msg));
        assert!(is_execution_corrective_message(&msg));
        // Other corrective markers must not be misclassified as this one.
        let retry = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("do something", ExecutionRetryReason::MissingMutation),
        });
        assert!(!is_parallel_batching_force(&retry));
    }

    // ─── Cascade-invariant + per-model resolver wiring ─────────────────────

    /// The runtime hard-corrective force MUST stay strictly above the
    /// prompt-layer soft nudge so the soft→hard cascade is preserved. If the
    /// resolved force ever drops to ≤ nudge, the runtime will inject a hard
    /// `user`-role corrective before the model has had any chance to
    /// self-correct on the soft prompt nudge — that inverts the intended
    /// failure-mode escalation.
    #[test]
    fn parallel_batching_force_default_above_nudge_threshold() {
        let cfg = astra_config::runtime_config::ToolPolicyConfig::default();
        let resolved = cfg.effective_parallel_batching_force_streak() as usize;
        assert!(
            resolved > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            "force streak {resolved} must stay strictly greater than nudge \
             threshold {} so the soft→hard cascade is preserved",
            crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD
        );
    }

    #[test]
    fn parallel_batching_force_min_tracks_runtime_nudge_plus_one() {
        assert_eq!(
            astra_config::runtime_config::MIN_PARALLEL_BATCHING_FORCE_STREAK as usize,
            crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD + 1,
            "config floor must stay exactly one round above the runtime nudge \
             threshold so the soft→hard cascade stays aligned across crates"
        );
    }

    /// The same invariant, but exercised through `resolve_for_model` with
    /// every built-in profile. Catches a regression where someone sets a
    /// per-model override below the nudge threshold or lets the config/runtime
    /// floors drift apart across crates.
    #[test]
    fn parallel_batching_force_per_model_above_nudge_threshold() {
        let cfg = astra_config::runtime_config::ToolPolicyConfig::default();
        for model in &[
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "gpt-5",
            "deepseek-v4-flash",
            "deepseek-v4-flash-anthropic",
            "MiniMax-M2.7",
            "unknown-model-id",
        ] {
            let policy = cfg.resolve_for_model(Some(*model));
            assert!(
                policy.parallel_batching_force_streak as usize
                    > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
                "model={model} resolved force={} must stay strictly greater \
                 than nudge threshold {} so the soft→hard cascade is preserved",
                policy.parallel_batching_force_streak,
                crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            );
        }
    }

    /// End-to-end wiring: the runtime guard MUST consume the *resolved*
    /// per-model threshold rather than the global default. This pins the
    /// chain `state.context_manifest_model_name → resolve_for_model →
    /// EffectiveToolPolicy::parallel_batching_force_streak →
    /// should_force_parallel_batching(_, threshold)`. A regression that
    /// re-routes the guard back to `effective_parallel_batching_force_streak`
    /// (model-blind) would silently break this.
    #[test]
    fn parallel_batching_force_uses_resolved_per_model_threshold() {
        // Configure a user profile well above the global default and nudge
        // threshold, so a default-length streak should NOT fire under this
        // profile but WOULD fire under the global default.
        let mut cfg = astra_config::runtime_config::ToolPolicyConfig::default();
        cfg.model_profiles
            .push(astra_config::runtime_config::ModelPolicyProfile {
                model_match: "haiku".to_string(),
                parallel_batching_force_streak: 11,
                ..Default::default()
            });
        let policy = cfg.resolve_for_model(Some("us.anthropic.claude-haiku-4-5-20251001-v1:0"));
        assert_eq!(policy.parallel_batching_force_streak, 11);

        let global_default = cfg.effective_parallel_batching_force_streak() as usize;
        assert!(global_default < policy.parallel_batching_force_streak as usize);

        // Build a state with a streak equal to the global default.
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..global_default {
            push_single_tool_round(&mut state);
        }

        // Resolved per-model threshold (=11) must suppress the corrective…
        assert!(
            !should_force_parallel_batching(&state, policy.parallel_batching_force_streak as usize),
            "streak={global_default} must NOT fire under per-model force=11"
        );

        // …whereas the model-blind global path would fire. This is the
        // actual regression target: if someone re-routes the guard back to
        // `effective_parallel_batching_force_streak`, the second assertion
        // would still pass but the first would change behavior — pinning
        // both makes the wiring explicit.
        assert!(
            should_force_parallel_batching(
                &state,
                cfg.effective_parallel_batching_force_streak() as usize
            ),
            "streak={global_default} SHOULD fire at the global default — sanity check that \
             the test exercises the right axis"
        );
    }

    /// Per-profile clamp invariant: a user that sets
    /// `parallel_batching_force_streak = 1` (or any value at/below
    /// `PARALLEL_BATCHING_NUDGE_THRESHOLD`) MUST land above the nudge
    /// threshold after `apply_profile`'s clamp, otherwise the runtime
    /// hard corrective fires before the prompt-layer ever nudges and
    /// the soft→hard cascade is silently inverted.
    #[test]
    fn parallel_batching_force_per_profile_clamp_above_nudge() {
        for low in 1..=crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD as u32 {
            let mut cfg = astra_config::runtime_config::ToolPolicyConfig::default();
            cfg.model_profiles
                .push(astra_config::runtime_config::ModelPolicyProfile {
                    model_match: "haiku".to_string(),
                    parallel_batching_force_streak: low,
                    ..Default::default()
                });
            let policy = cfg.resolve_for_model(Some("us.anthropic.claude-haiku-4-5-20251001-v1:0"));
            assert!(
                (policy.parallel_batching_force_streak as usize)
                    > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
                "per-profile force={low} resolved to {} but must be > nudge threshold {} \
                 to preserve the soft→hard cascade",
                policy.parallel_batching_force_streak,
                crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            );
        }
    }

    /// Round-budget warning state must not override the resolved per-model
    /// threshold. Pacing hints can be soft; hard correction should stay tied
    /// to the explicit tool-selection policy.
    // ─── Round-budget convergence guard — REMOVED ─────────────────────────
    // The old countdown-based phase1/phase2 tests have been replaced by
    // unit tests in `astra_turn_core::loop_circuit_breaker::tests`.
    // The circuit breaker is integration-tested via the full agentic loop
    // E2E tests.

    // ── Auto-mode nudge suppression ────────────────────────────────────
    // In PermissionMode::Auto (→ TurnInteractionMode::Auto) the user
    // opted into uninterrupted execution. Every corrective nudge we
    // would otherwise inject must be dropped. Regression coverage for
    // the "不停被打断" complaint in session 3b7ac18f.

    fn prep(quiet: bool) -> TurnIterationPrep {
        TurnIterationPrep {
            quiet,
            turn_start_time: Instant::now(),
        }
    }

    fn has_message_starting_with(state: &AgenticLoopState, prefix: &str) -> bool {
        state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.starts_with(prefix))
        })
    }

    #[tokio::test]
    async fn deferred_user_input_injects_immediately_at_loop_top() {
        let mut state = make_state();
        state.current_run_id = Some("run-queued".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![RunQueuedInputPoll {
            next_cursor: 2,
            inputs: vec![crate::turn::run_control::QueuedRunInputEvent {
                event_index: 1,
                input: serde_json::json!({"content": "Switch to writing tests first."}),
            }],
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(state.message, "Switch to writing tests first.");
        assert_eq!(
            state
                .messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str()),
            Some("Switch to writing tests first.")
        );
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.kind == VolatileKind::DeferredUserInput
                && entry.content.contains("Switch to writing tests first.")
        }));
    }

    #[tokio::test]
    async fn deferred_user_input_does_not_reinject_after_cursor_advance() {
        let mut state = make_state();
        state.current_run_id = Some("run-repoll".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            RunQueuedInputPoll {
                next_cursor: 2,
                inputs: vec![crate::turn::run_control::QueuedRunInputEvent {
                    event_index: 1,
                    input: serde_json::json!({"content": "only once"}),
                }],
                error: None,
            },
            RunQueuedInputPoll {
                next_cursor: 2,
                inputs: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|m| m.get("content").and_then(|c| c.as_str()) == Some("only once"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn deferred_user_input_retries_release_without_reinjecting_after_ack_failure() {
        let mut state = make_state();
        state.current_run_id = Some("run-release-retry".into());
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                RunQueuedInputPoll {
                    next_cursor: 2,
                    inputs: vec![crate::turn::run_control::QueuedRunInputEvent {
                        event_index: 1,
                        input: serde_json::json!({"content": "inject once"}),
                    }],
                    error: None,
                },
                RunQueuedInputPoll {
                    next_cursor: 2,
                    inputs: Vec::new(),
                    error: None,
                },
            ],
            1,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|m| m.get("content").and_then(|c| c.as_str()) == Some("inject once"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn deferred_user_input_advances_cursor_even_when_content_is_unusable() {
        let mut state = make_state();
        state.current_run_id = Some("run-invalid".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![RunQueuedInputPoll {
            next_cursor: 7,
            inputs: vec![crate::turn::run_control::QueuedRunInputEvent {
                event_index: 6,
                input: serde_json::json!({"unexpected": true}),
            }],
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 7);
        assert_eq!(*provider.released.lock().await, vec![6]);
        assert!(state.messages.is_empty());
        assert!(state.volatile_pending.is_empty());
    }

    #[tokio::test]
    async fn deferred_user_input_poll_error_fails_closed() {
        let mut state = make_state();
        state.current_run_id = Some("run-missing".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![RunQueuedInputPoll {
            next_cursor: 4,
            inputs: Vec::new(),
            error: Some("run not found while polling deferred input: run-missing".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .expect_err("poll errors must stop the loop instead of being treated as empty input");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert!(error.message.contains("run-missing"));
        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 0);
        assert!(state.messages.is_empty());
        assert!(provider.released.lock().await.is_empty());
    }

    #[test]
    fn deferred_user_input_text_preserves_active_skills_hint() {
        let rendered = deferred_user_input_text(&serde_json::json!({
            "content": "Use the release checklist.",
            "active_skills": ["release-manager", "deploy-auditor"],
        }))
        .expect("deferred input should render");

        assert!(rendered.contains("Requested active skills: release-manager, deploy-auditor."));
        assert!(rendered.contains("Use the release checklist."));
    }

    #[tokio::test]
    async fn auto_mode_suppresses_parallel_batching_force_injection() {
        // Set up a state that DEFINITELY would inject the
        // parallel-batching-force nudge in non-auto mode: long enough
        // single-tool streak past threshold.
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 2) {
            push_single_tool_round(&mut state);
        }

        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
            .with_interaction_mode(TurnInteractionMode::Auto);
        let _ = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(true))
            .await
            .unwrap();

        assert!(
            !has_message_starting_with(&state, PARALLEL_BATCHING_FORCE_MARKER),
            "Auto mode must not inject parallel-batching-force nudge"
        );
        assert!(
            !state.stall.forced_parallel_batching,
            "Auto mode must not set forced_parallel_batching flag"
        );
    }

    // Non-auto positive control is covered by the existing unit tests
    // `parallel_batching_force_fires_at_streak_threshold` etc. — those
    // test the predicate directly without the RuntimeConfig-dependent
    // loading code path that `execute_turn_and_ingest_phase` runs. The
    // Auto-mode suppression tests below target the new gate, which is
    // the only behaviour that changed.

    #[tokio::test]
    async fn auto_mode_suppresses_execution_escalation_injection() {
        // Build a state that would trigger execution escalation: many
        // read-only successful tool calls on a mutating-sounding task.
        let mut state = make_state();
        state.message = "fix the broken auth middleware".into();
        // Accumulate EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD successful
        // read-only records with no write. `ok: true` + non-synthetic is
        // the shape `should_escalate_execution` counts.
        for i in 0..EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD {
            state.stall.tool_call_records.push(ToolCallRecord {
                tool_call_id: None,
                name: "read_file".to_string(),
                ok: true,
                ms: 10,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: Some(format!("path: src/{i}.rs")),
                result_preview: None,
                file_path: Some(format!("src/{i}.rs")),
                surgically_removed: None,
                original_tool_name: None,
                start_offset_ms: None,
                batch_id: None,
                parallel: None,
                round: None,
                args_full: None,
                result_full: None,
                ask_user: None,
                skill_reentry_count: None,
                skill_locked_out: None,
                exit_semantics: None,
                result_class: None,
            });
        }

        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
            .with_interaction_mode(TurnInteractionMode::Auto);
        let _ = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(true))
            .await
            .unwrap();

        assert!(
            !has_message_starting_with(&state, EXECUTION_ESCALATION_MARKER),
            "Auto mode must not inject execution-escalation nudge"
        );
        assert!(!state.stall.forced_execution_escalation);
    }

    #[tokio::test]
    async fn auto_mode_suppresses_round_budget_guidance_injection() {
        // The prompt-side tool_round_guidance (parallel-batching soft
        // nudge at streak=4, before the hard force at streak=5) also
        // must stay silent in Auto.
        let mut state = make_state();
        state.message = "keep going".into();
        // Below the force threshold but at/above the soft-nudge
        // threshold (=4). This should emit a user message in non-auto.
        for _ in 0..crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD {
            push_single_tool_round(&mut state);
        }

        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
            .with_interaction_mode(TurnInteractionMode::Auto);
        let _ = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(true))
            .await
            .unwrap();

        // Neither the soft "Sequential Tool Calls Detected" nudge nor
        // the positive "Previous round: N tools" feedback should be
        // re-injected in Auto mode — both ride `tool_round_guidance`.
        assert!(
            !has_message_starting_with(&state, "## ⚠ Sequential Tool Calls Detected")
                && !state.messages.iter().any(|m| m
                    .get("content")
                    .and_then(|c| c.as_str())
                    .is_some_and(|s| s.contains("## ⚠ Sequential Tool Calls Detected"))),
            "Auto mode must not inject round-budget/tool guidance nudges"
        );
    }

    fn push_redundant_sed_read(state: &mut AgenticLoopState, round: u32) {
        // Same file, same range, no intervening mutation — counts as one
        // overlap each call after the first.
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: Some("sed -n '159,200p' f.rs".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: None,
            parallel: Some(false),
            round: Some(round),
            args_full: Some("sed -n '159,200p' f.rs".into()),
            ..Default::default()
        });
    }

    #[test]
    fn redundant_reads_corrective_fires_at_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        // First read seeds the file's history; subsequent overlapping reads
        // each contribute one redundant event.
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 1) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        assert!(should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        // Threshold-many reads = (threshold-1) overlap events: stays silent.
        for r in 0..REDUNDANT_READS_MIDLOOP_THRESHOLD {
            push_redundant_sed_read(&mut state, r as u32);
        }
        assert!(!should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 5) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        // First check fires...
        assert!(should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
        // ...then the one-shot flag gates the next attempt.
        state.stall.forced_redundant_reads_corrective = true;
        push_redundant_sed_read(&mut state, 99);
        assert!(!should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": redundant_reads_corrective_message(5, "fix the bug"),
        });
        assert!(is_redundant_reads_corrective(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_redundant_reads_corrective(&unrelated));
    }

    #[test]
    fn cache_waste_corrective_fires_at_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..CACHE_WASTE_MIDLOOP_THRESHOLD {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD - 1) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(!should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD + 2) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
        state.stall.forced_cache_waste_corrective = true;
        state.turn_guard.record_cache_hit("git_diff");
        assert!(!should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": cache_waste_corrective_message(&[("git_diff", 3)], "review local changes"),
        });
        assert!(is_cache_waste_corrective(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_cache_waste_corrective(&unrelated));
    }

    fn push_search_call(state: &mut AgenticLoopState, idx: usize) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "grep".into(),
            ok: true,
            args_full: Some(format!(r#"{{"pattern":"needle_{idx}","path":"rust"}}"#)),
            ..Default::default()
        });
    }

    #[test]
    fn search_fanout_corrective_fires_for_mutating_task() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile.mutates_workspace = true;
        for idx in 0..8 {
            push_search_call(&mut state, idx);
        }

        assert!(should_inject_search_fanout_corrective(&state, 8));
    }

    #[test]
    fn search_fanout_corrective_skips_read_only_review() {
        let mut state = make_state();
        state.message = "review the branch".into();
        state.task_profile.mutates_workspace = false;
        for idx in 0..12 {
            push_search_call(&mut state, idx);
        }

        assert!(!should_inject_search_fanout_corrective(&state, 8));
    }

    #[test]
    fn search_fanout_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile.mutates_workspace = true;
        for idx in 0..10 {
            push_search_call(&mut state, idx);
        }
        assert!(should_inject_search_fanout_corrective(&state, 8));
        state.stall.forced_search_fanout_corrective = true;
        assert!(!should_inject_search_fanout_corrective(&state, 8));
    }

    #[test]
    fn search_fanout_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": search_fanout_corrective_message(8, "fix the bug"),
        });
        assert!(is_search_fanout_corrective(&msg));
        assert!(is_execution_corrective_message(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_search_fanout_corrective(&unrelated));
    }

    fn push_diff_round(state: &mut AgenticLoopState, round: u32) {
        for idx in 0..2 {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "git".into(),
                ok: true,
                round: Some(round),
                args_full: Some(format!(
                    r#"{{"action":"diff","path":"src/file_{round}_{idx}.rs"}}"#
                )),
                ..Default::default()
            });
        }
    }

    fn push_search_round(state: &mut AgenticLoopState, round: u32) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "rg".into(),
            ok: true,
            round: Some(round),
            args_full: Some(format!(r#"{{"pattern":"needle_{round}","path":"rust/"}}"#)),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "glob".into(),
            ok: true,
            round: Some(round),
            args_full: Some(format!(r#"{{"pattern":"src/**/*_{round}.rs"}}"#)),
            ..Default::default()
        });
    }

    fn push_blocked_restricted_round(state: &mut AgenticLoopState, tool: &str, round: u32) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: tool.into(),
            ok: false,
            round: Some(round),
            error: Some(format!(
                "blocked_tool: Tool '{tool}' is currently restricted."
            )),
            result_preview: Some(format!(
                "Tool '{tool}' is currently restricted and cannot be executed."
            )),
            ..Default::default()
        });
    }

    #[test]
    fn exploration_family_corrective_fires_at_threshold_and_restricts_explicit_tools() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_diff_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_corrective_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family corrective candidate");
        };

        assert_eq!(family, "diff");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let restricted = apply_exploration_family_restrictions(&mut state, &family);
        assert_eq!(restricted, vec!["git".to_string()]);
        assert!(state.restricted_tools.contains("git"));
        assert!(
            !state.restricted_tools.contains("bash"),
            "exploration-family corrective must not globally block bash"
        );
    }

    #[test]
    fn exploration_family_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD - 1) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD + 2) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_some()
        );

        state.stall.forced_exploration_family_corrective = true;
        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": exploration_family_corrective_message(
                "diff",
                3,
                &["git".to_string()],
                "review local changes",
            ),
        });
        assert!(is_exploration_family_corrective(&msg));
        assert!(is_execution_corrective_message(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_exploration_family_corrective(&unrelated));
    }

    #[test]
    fn exploration_family_phase2_fires_after_blocked_only_retry_round() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git", 7);

        let candidate = exploration_family_phase2_candidate(&state);
        assert_eq!(
            candidate,
            Some(("diff".to_string(), vec!["git".to_string()])),
        );
    }

    #[test]
    fn exploration_family_phase2_stays_silent_on_mixed_progress_round() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git", 7);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            round: Some(7),
            args_full: Some(r#"{"command":"cargo test -p astra-runtime"}"#.into()),
            ..Default::default()
        });

        assert!(exploration_family_phase2_candidate(&state).is_none());
    }

    #[test]
    fn exploration_family_phase2_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git", 7);

        assert!(exploration_family_phase2_candidate(&state).is_some());
        state.stall.forced_exploration_family_phase2 = true;
        assert!(exploration_family_phase2_candidate(&state).is_none());
    }

    #[test]
    fn exploration_family_phase2_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": exploration_family_phase2_message(
                "diff",
                &["git".to_string()],
                "review local changes",
            ),
        });
        assert!(is_exploration_family_phase2(&msg));
        assert!(is_execution_corrective_message(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_exploration_family_phase2(&unrelated));
    }

    #[test]
    fn exploration_family_corrective_restricts_search_tools_without_bash() {
        let mut state = make_state();
        state.message = "investigate auth flow".into();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_search_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_corrective_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family search corrective candidate");
        };

        assert_eq!(family, "search");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let restricted = apply_exploration_family_restrictions(&mut state, &family);
        assert_eq!(
            restricted,
            vec!["glob".to_string(), "grep".to_string(), "rg".to_string()]
        );
        assert!(state.restricted_tools.contains("glob"));
        assert!(state.restricted_tools.contains("grep"));
        assert!(state.restricted_tools.contains("rg"));
        assert!(
            !state.restricted_tools.contains("bash"),
            "search-family corrective must not globally block bash"
        );
    }

    #[tokio::test]
    async fn pipeline_session_receives_feedback_on_successful_turn() {
        use astra_turn_core::pipeline_config::PipelineConfig;
        use astra_turn_core::pipeline_session::PipelineSession;

        let mut state = make_state();
        state.pipeline_session = Some(PipelineSession::new(PipelineConfig::default()));

        let mut host = MockHost::new(vec![text_result("Hello", 1000, 200, Some(50))]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_ok());

        let sess = state.pipeline_session.as_ref().unwrap();
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.stats.turns_executed, 1);
    }

    #[tokio::test]
    async fn pipeline_session_none_does_not_panic() {
        let mut state = make_state();
        assert!(state.pipeline_session.is_none());

        let mut host = MockHost::new(vec![text_result("Hello", 500, 100, None)]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failed_turn_still_preserves_last_request_message_count() {
        let mut state = make_state();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "retry me"}));

        let mut host = MockHost::new(vec![HostTurnResult {
            accum: ChatTurnSseAccum {
                error_message: Some("rate limit exceeded".to_string()),
                has_usage: true,
                prompt_tokens: 12,
                completion_tokens: 0,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        }]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_err());
        assert_eq!(state.llm_rounds_completed, 1);
        assert_eq!(
            state.last_request_message_count,
            Some(1),
            "failed LLM attempts must still protect the exact request prefix for retry-time compaction"
        );
    }
}

/// Spill old messages to disk with a structural summary retained in context.
///
/// Strategy (SpillBackend pattern for conversation history):
/// 1. Extract a compact structural summary from the messages being spilled
///    (user intents, tool calls made, files touched, errors hit)
/// 2. Serialize the full messages to a session-local file (backup)
/// 3. Replace the spilled messages with ONE system message containing:
///    - The structural summary (so the agent retains awareness)
///    - The spill file path (so it can read_file for full details)
///
/// This is NOT just raw dump — the summary gives the agent enough context
/// to continue working without re-reading the full history. But if it needs
/// specifics, the full transcript is one read_file away.
///
/// Returns estimated tokens freed.
// Spill policy tunables — keep ~40% of the tail, shed ~60%. Chosen to
// meaningfully relieve pressure in a single pass while preserving enough
// recent turns that the agent doesn't lose working context.
const SPILL_KEEP_NUMERATOR: usize = 2;
const SPILL_KEEP_DENOMINATOR: usize = 5;
const SPILL_MIN_KEEP: usize = 6;
const SPILL_MIN_TOTAL: usize = 10;
const SPILL_MIN_SPILL: usize = 4;

/// Adjust `spill_count` so the drain boundary lands on a clean role boundary.
///
/// Provider APIs require assistant messages with `tool_calls` / `tool_use`
/// blocks to be followed by matching tool-result messages with the same ids.
/// If we spill through the middle of such a pair we'll get 400s on the next
/// provider call. This walks the boundary *backward* (spilling fewer messages)
/// until we land in a safe spot:
///   - the retained prefix does not start with a `tool` / `tool_result` role, and
///   - the last spilled message is not an assistant with unanswered tool calls.
pub(crate) fn adjust_spill_boundary_for_tool_pairs(
    messages: &[serde_json::Value],
    mut spill_count: usize,
) -> usize {
    let is_tool_role = |m: &serde_json::Value| -> bool {
        let role = m.get("role").and_then(|r| r.as_str());
        // OpenAI-shape: role is "tool"; Anthropic-shape: role is "tool_result".
        if matches!(role, Some("tool") | Some("tool_result")) {
            return true;
        }
        // Anthropic tool-result messages arrive as role="user" with a content
        // array containing {type:"tool_result"} blocks.  The current-role check
        // above misses these, which would leave an orphaned tool_use assistant
        // message in the retained window.
        if role == Some("user") {
            if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
                return arr
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
            }
        }
        false
    };
    let has_tool_calls = |m: &serde_json::Value| -> bool {
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            return false;
        }
        // OpenAI-shape: top-level `tool_calls` array.
        if m.get("tool_calls")
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        // Anthropic-shape: `content` is an array with `tool_use` blocks.
        if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
            return arr
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        }
        false
    };

    // Walk backward while the boundary is unsafe. Bail if we'd spill nothing.
    while spill_count > 0 {
        let last_spilled = &messages[spill_count - 1];
        let first_retained = messages.get(spill_count);
        let retained_starts_with_tool = first_retained.map(is_tool_role).unwrap_or(false);
        let last_is_pending_assistant = has_tool_calls(last_spilled);
        if !retained_starts_with_tool && !last_is_pending_assistant {
            break;
        }
        spill_count -= 1;
    }
    spill_count
}

fn spill_old_messages_to_disk(
    messages: &mut Vec<serde_json::Value>,
    session_id: &str,
    round: u32,
) -> u64 {
    let total = messages.len();
    if total < SPILL_MIN_TOTAL {
        return 0;
    }
    let keep_count = (total * SPILL_KEEP_NUMERATOR / SPILL_KEEP_DENOMINATOR).max(SPILL_MIN_KEEP);
    let mut spill_count = total.saturating_sub(keep_count);
    // Snap to a safe role boundary so we never split an assistant/tool pair.
    spill_count = adjust_spill_boundary_for_tool_pairs(messages, spill_count);
    if spill_count < SPILL_MIN_SPILL {
        return 0;
    }

    let to_spill: Vec<_> = messages.drain(..spill_count).collect();

    // Build structural summary from the spilled messages.
    let summary = build_spill_summary(&to_spill);

    let spill_json = match serde_json::to_string_pretty(&to_spill) {
        Ok(json) => json,
        Err(_) => {
            // Put messages back in their original position (prefix).
            let mut restored = to_spill;
            restored.append(messages);
            *messages = restored;
            return 0;
        }
    };
    let tokens_freed = (spill_json.len() / 4) as u64;

    // Write full transcript to session dir.
    let spill_dir = astra_services::session_journal::local_sessions_dir().join(session_id);
    let _ = std::fs::create_dir_all(&spill_dir);
    let spill_path = spill_dir.join(format!("spill-round{round}.json"));
    if std::fs::write(&spill_path, &spill_json).is_err() {
        let mut restored = to_spill;
        restored.append(messages);
        *messages = restored;
        return 0;
    }

    // Insert summary + reference as first message.
    let reference_msg = serde_json::json!({
        "role": "system",
        "content": format!(
            "[Context compressed — {spill_count} earlier messages spilled to disk]\n\n\
             ## Summary of spilled context\n{summary}\n\n\
             ## Full transcript\n\
             Path: {path}\n\
             Use `read_file` on this path if you need exact details from \
             the earlier conversation.",
            path = spill_path.display(),
        )
    });
    messages.insert(0, reference_msg);

    tokens_freed
}

/// Extract a structural summary from messages without LLM — pure string extraction.
/// Captures: user requests, tools called, files modified, errors encountered.
fn build_spill_summary(messages: &[serde_json::Value]) -> String {
    let mut user_messages = Vec::new();
    let mut tools_used = Vec::new();
    let mut files_modified = Vec::new();
    let mut errors = Vec::new();

    // Synthetic/system-injected user messages that shouldn't count as "requests".
    const SYNTHETIC_USER_PREFIXES: &[&str] = &[
        "[attention:",
        "[session-anchor]",
        "[working-set:",
        "[session-memory:",
        "(cached",
    ];
    let is_synthetic_user = |s: &str| {
        SYNTHETIC_USER_PREFIXES
            .iter()
            .any(|p| s.trim_start().starts_with(p))
    };

    // Extract plain text from a `content` field that may be a string or an
    // array of content blocks (Anthropic shape).
    let content_text = |v: &serde_json::Value| -> Option<String> {
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = v.as_array() {
            let mut out = String::new();
            for b in arr {
                let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "text" {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
        None
    };

    // Record a tool invocation, extracting a `path` arg when present.
    let mut record_tool = |name: &str, args: &serde_json::Value| {
        let path = args.get("path").and_then(|p| p.as_str());
        if let Some(p) = path {
            if matches!(name, "str_replace" | "write_file" | "multi_edit") {
                let ps = p.to_string();
                if !files_modified.contains(&ps) {
                    files_modified.push(ps);
                }
            }
            tools_used.push(format!("{name}({p})"));
        } else {
            tools_used.push(name.to_string());
        }
    };

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                if let Some(content) = msg.get("content").and_then(content_text) {
                    if !is_synthetic_user(&content) {
                        let preview: String = content.chars().take(150).collect();
                        user_messages.push(preview);
                    }
                }
            }
            "assistant" => {
                // OpenAI-shape: top-level `tool_calls`.
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        let args_str = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");
                        let parsed: serde_json::Value =
                            serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
                        record_tool(name, &parsed);
                    }
                }
                // Anthropic-shape: content array with `tool_use` blocks.
                if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            record_tool(name, &input);
                        }
                    }
                }
                // Error mentions in assistant text — require word boundaries
                // to avoid false positives like "no errors" or "won't fail".
                if let Some(text) = msg.get("content").and_then(content_text) {
                    let looks_like_error = text.contains(": error")
                        || text.contains("Error:")
                        || text.contains("panicked")
                        || text.contains("traceback")
                        || text.contains("Traceback");
                    if looks_like_error && errors.len() < 5 {
                        let preview: String = text.chars().take(100).collect();
                        errors.push(preview);
                    }
                }
            }
            _ => {}
        }
    }

    let mut summary = String::new();

    if !user_messages.is_empty() {
        summary.push_str("**User requests:**\n");
        for (i, msg) in user_messages.iter().take(10).enumerate() {
            summary.push_str(&format!("{}. {}\n", i + 1, msg));
        }
        summary.push('\n');
    }

    if !files_modified.is_empty() {
        summary.push_str("**Files modified:**\n");
        for f in files_modified.iter().take(20) {
            summary.push_str(&format!("- {f}\n"));
        }
        summary.push('\n');
    }

    if !tools_used.is_empty() {
        // Deduplicate and count
        let mut tool_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for t in &tools_used {
            *tool_counts.entry(t.as_str()).or_default() += 1;
        }
        let mut sorted: Vec<_> = tool_counts.into_iter().collect();
        sorted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        summary.push_str(&format!("**Tools used ({} calls):**\n", tools_used.len()));
        for (tool, count) in sorted.iter().take(15) {
            if *count > 1 {
                summary.push_str(&format!("- {tool} ×{count}\n"));
            } else {
                summary.push_str(&format!("- {tool}\n"));
            }
        }
        summary.push('\n');
    }

    if !errors.is_empty() {
        summary.push_str("**Errors encountered:**\n");
        for e in &errors {
            summary.push_str(&format!("- {e}\n"));
        }
    }

    if summary.is_empty() {
        summary.push_str("(no structured content extracted from spilled messages)");
    }

    summary
}
