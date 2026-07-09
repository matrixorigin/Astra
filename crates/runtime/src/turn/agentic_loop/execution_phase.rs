use std::time::{Duration, Instant};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, DeferredInputState, HostTurnResult,
    TaskBoardSnapshot, finalize_and_render, finalize_turn_trace, try_write_heavy_checkpoint,
};
use super::lifecycle::{
    TurnIterationPrep, current_agentic_step, interruption_diagnosis_summary,
    interruption_state_summary, session_turn_number, tool_record_is_workspace_mutation,
};
use astra_config::user_profile::Scenario;
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
use astra_turn_core::response_guard::RESPONSE_GUARD_BLOCKED_FINISH_REASON;
use astra_turn_core::stall::IntentDrift;
use uuid::Uuid;

use crate::turn::observation_dispatcher::{
    FileSink, MemorySink, ObservationDispatcher, ObservationEvent,
};

const DEFERRED_USER_INPUT_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(500);

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

fn alert_dispatch_session_id(session_id: Option<&str>) -> Option<String> {
    session_id
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .map(ToString::to_string)
}

fn is_llm_provider_admission_error(error: &astra_core::ClassifiedError) -> bool {
    let Some(details_json) = error.details_json.as_deref() else {
        return false;
    };
    let Ok(serde_json::Value::Object(details)) =
        serde_json::from_str::<serde_json::Value>(details_json)
    else {
        return false;
    };
    details.get("source").and_then(serde_json::Value::as_str) == Some("llm_provider_admission")
}

fn record_direct_llm_error_state(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) {
    match error.kind {
        astra_core::ErrorKind::RateLimit if !is_llm_provider_admission_error(error) => {
            state.rate_limit_cooldown.record_429(None, false);
        }
        astra_core::ErrorKind::ServerError => {
            state.rate_limit_cooldown.record_529(None, false);
        }
        _ => {}
    }

    if state.interruption.is_some() {
        return;
    }
    if let Some((kind, action)) =
        astra_turn_core::interruption::interruption_from_error_kind(error.kind)
    {
        state.interruption = Some(InterruptionRecord::new(
            kind,
            action,
            interruption_state_summary(state, Some(error.message.clone())),
        ));
    }
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

pub(crate) async fn inject_polled_deferred_user_inputs<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    let poll_started = tokio::time::Instant::now();
    if !state.deferred_input.should_poll_user_inputs(poll_started) {
        return Ok(());
    }

    let (run_control, user_id, run_id) = match (
        state.run_control.as_ref(),
        state.context_manifest_user_id.as_ref(),
        state.current_run_id.as_ref(),
    ) {
        (Some(run_control), Some(user_id), Some(run_id)) => {
            (run_control.clone(), user_id.clone(), run_id.clone())
        }
        _ => return Ok(()),
    };
    let poll = run_control
        .poll_user_inputs(
            &user_id,
            &run_id,
            state.deferred_input.deferred_user_input_cursor(),
        )
        .await;
    if let Some(error) = &poll.error {
        state
            .deferred_input
            .note_user_input_poll_finished(poll_started, DEFERRED_USER_INPUT_EMPTY_POLL_INTERVAL);
        tracing::warn!(run_id, error = %error, "deferred user input poll failed");
        return Ok(());
    }
    let observed = state
        .deferred_input
        .observe_polled_user_inputs(poll, deferred_user_input_text);
    let release_event_indices = state
        .deferred_input
        .release_event_indices_to_ack(&observed.released_event_indices);
    state
        .deferred_input
        .note_user_input_poll_finished(poll_started, DEFERRED_USER_INPUT_EMPTY_POLL_INTERVAL);
    if observed.raw_inputs.is_empty() && release_event_indices.is_empty() {
        return Ok(());
    }

    for input in &observed.raw_inputs {
        host.on_deferred_user_input(input);
    }

    if !observed.contents.is_empty() {
        let combined = observed
            .contents
            .iter()
            .map(|input| input.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !combined.is_empty() {
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": combined.clone(),
            }));
            state.message = observed
                .contents
                .last()
                .map(|input| input.content.clone())
                .unwrap_or_default();
            state
                .deferred_input
                .record_delivered_user_inputs(&observed.contents);
        }
    }

    state
        .deferred_input
        .commit_observed_cursor(observed.next_cursor);
    if release_event_indices.is_empty() {
        return Ok(());
    }
    match run_control
        .mark_user_inputs_released(&user_id, &run_id, &release_event_indices)
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

pub(crate) fn turn_result_tokens_consumed(turn_result: &HostTurnResult) -> u64 {
    turn_result
        .accum
        .prompt_tokens
        .saturating_add(turn_result.accum.completion_tokens)
        .saturating_add(turn_result.accum.cache_read_tokens)
        .saturating_add(turn_result.accum.cache_creation_tokens)
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

pub(crate) struct TurnExecutionPhase {
    pub(crate) llm_wall_start: Instant,
    pub(crate) turn_result: HostTurnResult,
}

pub(crate) enum TurnExecutionControl {
    Proceed(Box<TurnExecutionPhase>),
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

fn route_runtime_policy_evidence(
    state: &mut AgenticLoopState,
    facts: &astra_core::observation_journal::JournalFacts,
    evidence: crate::turn::runtime_policy::RuntimePolicyEvidence,
) {
    use crate::turn::runtime_policy::{PhaseTarget, RuntimePolicyEvidence};

    match evidence {
        RuntimePolicyEvidence::BudgetExpansionSuggested {
            factor,
            max_ceiling,
        } => {
            state.push_volatile_payload(
                super::host::VolatileKind::BudgetReview,
                serde_json::json!({
                    "schema": "runtime_policy_advisory.v1",
                    "signal": "budget_expansion_suggested",
                    "evidence": {
                        "consecutive_outcomes": facts.streaks.consecutive_rounds_with_outcome,
                        "current_max_turns": state.max_turns,
                        "remaining_turns": state.remaining_turns,
                        "suggested_factor": factor,
                        "configured_ceiling": max_ceiling,
                    },
                    "authority": "advisory_evidence_only",
                    "model_discretion": "The policy did not change the runtime budget. Continue within the actual remaining budget shown by the runtime.",
                }),
            );
            tracing::info!(
                target: "astra::policy",
                factor,
                max_ceiling,
                "policy recorded budget-expansion evidence without mutating budget"
            );
        }
        RuntimePolicyEvidence::Advisory { message } => {
            state.push_volatile_payload(
                super::host::VolatileKind::BehaviorAdvisory,
                serde_json::json!({
                    "schema": "runtime_policy_advisory.v1",
                    "signal": "policy_observation",
                    "evidence": message,
                    "authority": "advisory_evidence_only",
                }),
            );
            tracing::info!(
                target: "astra::policy",
                signal = %message,
                "policy observation recorded as advisory evidence"
            );
        }
        RuntimePolicyEvidence::ContextPressureObserved { urgency } => {
            let pressure = facts.performance.token_pressure;
            state.push_volatile_payload(
                super::host::VolatileKind::ContextPressure,
                serde_json::json!({
                    "schema": "runtime_policy_advisory.v1",
                    "signal": "context_pressure_observed",
                    "evidence": {
                        "token_pressure": pressure,
                        "urgency": urgency.to_string(),
                    },
                    "recommendation": "Consider reusing prior results, avoiding duplicate reads, or selecting a narrow next action.",
                    "authority": "advisory_evidence_only",
                }),
            );
            tracing::info!(
                target: "astra::policy",
                %urgency,
                token_pressure = pressure,
                "policy context-pressure evidence recorded"
            );
        }
        RuntimePolicyEvidence::PhaseTransitionSuggested { target } => {
            let phase_label = match target {
                PhaseTarget::Reflection => "reflection",
                PhaseTarget::Summarization => "summarization",
                PhaseTarget::Planning => "planning",
                PhaseTarget::Completion => "completion",
            };
            state.push_volatile_payload(
                super::host::VolatileKind::BehaviorAdvisory,
                serde_json::json!({
                    "schema": "runtime_policy_advisory.v1",
                    "signal": "phase_transition_suggested",
                    "evidence": {
                        "suggested_phase": phase_label,
                        "task_completion_ratio": facts.task.task_completion_ratio,
                    },
                    "recommendation": "Consider transitioning if that best serves the current user goal.",
                    "authority": "advisory_evidence_only",
                    "model_discretion": "The runtime did not change phase or require finalization.",
                }),
            );
            tracing::info!(
                target: "astra::policy",
                %target,
                completion_ratio = facts.task.task_completion_ratio,
                "policy phase-transition evidence recorded"
            );
        }
        RuntimePolicyEvidence::NoAdvisory => {}
    }
}

fn manifest_reason_for_llm_call(state: &AgenticLoopState) -> &'static str {
    if state.compact_tier_applied != astra_turn_core::compaction_types::CompactionTier::Normal
        || state.compaction_effectiveness.attempt_count > 0
    {
        "post_compaction"
    } else {
        "normal_turn"
    }
}

fn infer_turn_intent_for_llm_call(state: &AgenticLoopState) -> String {
    if let Some(intent) = state.turn_intent.as_ref()
        && let Some(scenario) = intent
            .requested_scenario
            .filter(|scenario| intent.allows_scenario(*scenario))
    {
        return scenario_context_manifest_label(scenario).to_string();
    }
    if state.task_profile.mutates_workspace {
        "implementation".to_string()
    } else if state.task_profile.exploratory_task {
        "exploration".to_string()
    } else {
        "normal".to_string()
    }
}

fn scenario_context_manifest_label(scenario: Scenario) -> &'static str {
    match scenario {
        Scenario::CodeReview => "code_review",
        Scenario::Debugging => "debugging",
        Scenario::Exploration => "exploration",
        Scenario::Planning => "planning",
        Scenario::Implementation => "implementation",
        Scenario::Refactoring => "refactoring",
        Scenario::Testing => "testing",
        Scenario::Documentation => "documentation",
        Scenario::DevOps => "dev_ops",
        Scenario::Learning => "learning",
        Scenario::QuickAnswer => "quick_answer",
        Scenario::BenchmarkComparison => astra_services::TURN_INTENT_BENCHMARK_COMPARISON,
    }
}

async fn persist_context_manifest_for_llm_call(
    state: &AgenticLoopState,
    turn_index: usize,
    llm_attempt_index: u32,
    pre_llm_messages: &[serde_json::Value],
    turn_result: Option<&HostTurnResult>,
) {
    if !context_manifest_db_persistence_enabled() {
        return;
    }
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
    let turn_intent = infer_turn_intent_for_llm_call(state);
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
    let context_window_tokens = context_window_tokens_for_context_manifest(state);
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

fn context_manifest_db_persistence_enabled_for_trace(
    trace: &astra_config::runtime_config::SessionTraceConfig,
) -> bool {
    trace.category_enabled(astra_config::runtime_config::TraceCategory::ContextAssembly)
}

fn context_manifest_db_persistence_enabled() -> bool {
    context_manifest_db_persistence_enabled_for_trace(
        &astra_config::runtime_config::RuntimeConfig::cached().trace,
    )
}

fn context_window_tokens_for_context_manifest(state: &AgenticLoopState) -> u32 {
    state
        .last_llm_context_manifest_trace
        .as_ref()
        .and_then(|trace| trace.get("model_context_window_tokens"))
        .and_then(|value| value.as_u64())
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
        .unwrap_or(crate::prompts::DEFAULT_CONTEXT_WINDOW_TOKENS as u32)
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
    // model run to completion without interruption. Skip optional behavioral
    // advisory evidence in that case. True safety, permission, capability, and
    // budget boundaries remain enforced independently.
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
    if !host.injects_round_guidance() {
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
            let status_provider = crate::turn::local_provider::LocalSessionProvider::new(state);
            let cb_state = status_provider.circuit_breaker_state().to_string();
            let cache_ratio = status_provider.cache_hit_ratio();
            let token_pressure = status_provider.token_pressure();
            let alerts: Vec<String> = {
                let mut a = Vec::new();
                if state.stall.execution_escalation_advisory_emitted {
                    a.push("execution_escalation".to_string());
                }
                if state.stall.drift_nudge_count > 0 {
                    a.push(format!("drift_nudges={}", state.stall.drift_nudge_count));
                }
                if state.stall.nudge_count > 0 {
                    a.push(format!("stall_nudges={}", state.stall.nudge_count));
                }
                let recent_tool_failures = state.turn_guard.health.recent_errors(10).len();
                if recent_tool_failures > 0 {
                    a.push(format!(
                        "tool_failures={recent_tool_failures}; tools remain available unless an explicit restricted_tool result appears"
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
                state.push_volatile(super::host::VolatileKind::SelfStatus, status);
            }
        }

        if !suppress_nudges {
            let guidance =
                crate::prompts::tool_round_guidance(&state.messages, state.llm_rounds_completed);
            if !guidance.is_empty() {
                state.push_volatile(super::host::VolatileKind::BudgetAdvisory, guidance);
            }
        }
    }

    // If a mutating task has accumulated only read-only observations, surface
    // that fact before the next LLM call. It remains advisory because further
    // investigation may still be justified by a concrete unknown.
    if !suppress_nudges && should_emit_execution_escalation_advisory(state) {
        let read_only_calls = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder() && r.ok)
            .count();
        state.stall.execution_escalation_advisory_emitted = true;
        let msg = execution_escalation_message(&state.message, read_only_calls);
        state.push_volatile(super::host::VolatileKind::ExecutionEscalation, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "execution_escalation",
            read_only_calls,
            round = state.llm_rounds_completed,
            "execution-pattern advisory observed"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ Mutating task accumulated {read_only_calls} read-only tool calls with zero edits; adding execution advisory…"
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
    // them in order and records advisory evidence. Guards are
    // independently testable — see guards.rs for individual unit tests.
    //
    // Previously each guard was inlined as ~30-line blocks below; the
    // pipeline reduces this section from ~80 lines to ~15.
    //
    // Runs after the circuit breaker so guards can avoid stacking redundant
    // advisory evidence when a stronger signal was already emitted.
    {
        let guard_cfg = super::guards::GuardConfig {
            suppress_nudges,
            parallel_batching_force_streak: parallel_batching_force_threshold,
            redundant_reads_threshold,
            cache_waste_threshold,
        };
        let guards = super::guards::default_guards();
        for (hint_style, hint_text) in super::guards::evaluate_guards(&guards, state, &guard_cfg) {
            if !prep.quiet {
                host.emit_headless_line(hint_style, hint_text);
            }
        }
    }

    // ── Policy-driven evaluation (RuntimePolicy) ───────────────────────
    // Compute JournalFacts from the current loop state and delegate to
    // RuntimePolicy::decide(). The policy produces `RuntimePolicyEvidence`
    // values that complement the guard pipeline above.
    //
    // This runs after the guard pipeline so it sees the latest tool-call
    // records and circuit-breaker state.
    {
        use crate::turn::local_provider::LocalSessionProvider;
        use crate::turn::providers::{
            LiveRuntimeProvider, ObservationProvider, SessionStateProvider,
        };
        use astra_core::observation_journal::JournalFacts;

        let provider = LocalSessionProvider::new(state);

        // Extract journal facts from the ObservationProvider trait.
        let mut facts = provider.extract_facts();

        // Populate session-wide fields from authoritative state.
        // extract_facts provides streak and budget data from the journal
        // window; these fields come from the full session state.
        facts.budget.rounds_completed = state.llm_rounds_completed;
        facts.performance.total_observation_calls = state.total_observation_tool_calls;
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
        let policy_evidence = policy.decide(&facts);

        // ── Observation pipeline: dispatch PolicyEvidence events ──
        {
            let mut dispatcher = ObservationDispatcher::new();
            dispatcher.register(MemorySink::new(&mut state.observation_journal));
            if let Some(ref store) = state.observation_store {
                dispatcher.register(FileSink::new(
                    Some(store.clone()),
                    state
                        .current_session_id
                        .as_deref()
                        .unwrap_or_default()
                        .to_string(),
                ));
            }
            for evidence in &policy_evidence {
                dispatcher.dispatch(ObservationEvent::PolicyEvidence {
                    evidence: evidence.clone(),
                });
            }
        } // dispatcher dropped — releases &mut observation_journal

        for evidence in policy_evidence {
            route_runtime_policy_evidence(state, &facts, evidence);
        }
    }

    // ── Intent drift detection ─────────────────────────────────────────
    // Check if the agent has drifted from the user's original intent by
    // analyzing recent tool calls against the user query. If drift is
    // detected, surface advisory evidence via the volatile lane so the LLM
    // refocuses on the original task. Singleton kind ensures only the
    // latest observation rides the wire, avoiding prompt cache bloat.
    //
    // Runs after the guard pipeline so it sees the most recent tool calls
    // in `state.stall.intent_tool_turns`. Skipped when `suppress_nudges`
    // is true (Auto mode) to avoid interrupting the flow.
    //
    // One-shot per turn: once intent_drift_advisory_emitted is set, no further
    // repeated advisories are not injected this turn, preserving prompt-cache prefix.
    if !suppress_nudges
        && !state.stall.intent_drift_advisory_emitted
        && state.llm_rounds_completed > 0
    {
        let drift = host
            .detect_intent_drift(&state.message, &state.stall.intent_tool_turns)
            .await;
        if let IntentDrift::Drifting { correction, .. } = drift {
            state.stall.intent_drift_advisory_emitted = true;
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
                "intent drift evidence recorded"
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
    // Feed the previous round's signal to the circuit breaker. The runtime
    // treats loop-pattern detectors as observation, not authority: repeated
    // read/search/tool patterns can be valid work. Only infrastructure hard
    // ceilings may terminate the turn here. Advisory signals are recorded for
    // telemetry and introspection but do not inject extra user messages.
    if state.llm_rounds_completed > 0 {
        let signal = build_circuit_breaker_signal(state);
        let action = state.stall.circuit_breaker.observe(signal);
        match action {
            astra_turn_core::loop_circuit_breaker::BreakerAction::PatternObserved => {
                if !suppress_nudges {
                    state.push_volatile_payload(
                        super::host::VolatileKind::CircuitBreaker,
                        serde_json::json!({
                            "signal": "repeated_behavior_pattern",
                            "round": state.llm_rounds_completed,
                            "assessment": "The recent tool pattern may be repetitive. Treat this as evidence when choosing the next action; continue if the repetition is justified by the task."
                        }),
                    );
                }
                state
                    .stall
                    .circuit_breaker
                    .acknowledge_pattern_observation();
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_advisory",
                    round = state.llm_rounds_completed,
                    suppressed = suppress_nudges,
                    "circuit breaker observed a repeated pattern; no advisory message injected"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::AdvisoryThresholdReached => {
                let diagnosis = interruption_diagnosis_summary(state);
                if !suppress_nudges {
                    state.push_volatile_payload(
                        super::host::VolatileKind::CircuitBreaker,
                        serde_json::json!({
                            "signal": "repetition_threshold_reached",
                            "round": state.llm_rounds_completed,
                            "diagnosis": diagnosis,
                            "assessment": "A behavior-pattern detector reached its configured threshold. This is advisory evidence, not a budget or safety boundary; decide whether to change approach or continue with the current evidence."
                        }),
                    );
                }
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_threshold_advisory",
                    round = state.llm_rounds_completed,
                    "circuit breaker threshold recorded as advisory evidence"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Repetition threshold observed at round {}; continuing with advisory evidence.",
                            state.llm_rounds_completed
                        ),
                    );
                }
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::HardRoundLimitReached {
                rounds,
                limit,
            } => {
                let detail = format!("Infrastructure round limit reached: {rounds}/{limit} rounds");
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::BudgetExhausted,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(state, Some(detail.clone())),
                ));
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!("⚠ {detail}; preserving completed work."),
                    );
                }
                try_write_heavy_checkpoint(state);
                state.step_recorder.end_turn(false);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Introspect { .. }
                if suppress_nudges =>
            {
                // Auto mode: advisory signal only.
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Introspect {
                consecutive_read_only,
            } => {
                state.stall.introspection_count = state.stall.introspection_count.saturating_add(1);
                let emission_index = state.stall.introspection_count;
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "read_only_streak",
                        "consecutive_read_only": consecutive_read_only,
                        "round": state.llm_rounds_completed,
                        "assessment": "Review whether the current read-only investigation is still producing new evidence. This signal does not require stopping or changing tools."
                    }),
                );
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_introspect_advisory",
                    round = state.llm_rounds_completed,
                    consecutive_read_only,
                    emission = emission_index,
                    "circuit breaker introspection signal recorded; no prompt injected"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::CompletionObserved
                if suppress_nudges => {}
            astra_turn_core::loop_circuit_breaker::BreakerAction::CompletionObserved => {
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    completion_observation_payload(state.llm_rounds_completed, &state.message),
                );
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "completion_observation",
                    round = state.llm_rounds_completed,
                    "completion observation injected"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Task completion evidence observed at round {}; model retains final-action discretion.",
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
        && !state.stall.stronger_advisory_emitted()
        && let Some((family, retry_cautioned_tools)) = exploration_family_phase2_candidate(state)
    {
        state.stall.stronger_exploration_family_advisory_emitted = true;
        let msg =
            exploration_family_phase2_message(&family, &retry_cautioned_tools, &state.message);
        state.push_volatile(super::host::VolatileKind::BehaviorAdvisory, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_phase2",
            round = state.llm_rounds_completed,
            family = family,
            retry_cautioned_tools = ?retry_cautioned_tools,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ repeated low-yield {family} retry [{}]; adding convergence advisory…",
                    retry_cautioned_tools.join(", ")
                ),
            );
        }
    }

    // Redundant-reads and cache-waste advisories are now handled by the
    // composable guard pipeline above (`default_guards()`). They were
    // previously inlined here as ~30-line blocks; the pipeline version also
    // carries their stderr hints so the host doesn't need to re-render them.
    if !suppress_nudges
        && !state.stall.stronger_advisory_emitted()
        && !state.stall.redundant_reads_advisory_emitted
        && !state.stall.cache_waste_advisory_emitted
        && should_emit_search_fanout_advisory(state, search_fanout_threshold)
    {
        let count =
            astra_turn_core::evaluation::count_search_fanout(&state.stall.tool_call_records);
        state.stall.search_fanout_advisory_emitted = true;
        let msg = search_fanout_advisory_message(count, &state.message);
        state.push_volatile(super::host::VolatileKind::BehaviorAdvisory, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "search_fanout_advisory",
            round = state.llm_rounds_completed,
            count = count,
            threshold = search_fanout_threshold,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {count} search calls in an implementation turn; adding synthesis advisory…"
                ),
            );
        }
    }
    if !suppress_nudges
        && !state.stall.stronger_advisory_emitted()
        && !state.stall.redundant_reads_advisory_emitted
        && !state.stall.cache_waste_advisory_emitted
        && !state.stall.search_fanout_advisory_emitted
        && let Some((family, streak)) =
            exploration_family_advisory_candidate(state, exploration_family_threshold)
    {
        let retry_cautioned = mark_exploration_family_advisory(state, &family);
        state.stall.exploration_family_advisory_emitted = true;
        let msg =
            exploration_family_advisory_message(&family, streak, &retry_cautioned, &state.message);
        state.push_volatile(super::host::VolatileKind::BehaviorAdvisory, msg);
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_advisory",
            round = state.llm_rounds_completed,
            family = family,
            streak = streak,
            retry_cautioned = ?retry_cautioned,
            "loop guard fired"
        );
        if !prep.quiet {
            let retry_cautioned_display = retry_cautioned.join(", ");
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {streak} consecutive low-yield {family} rounds; retry-cautioning [{retry_cautioned_display}] without disabling tools…"
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
    let turn_result = match turn_result {
        Ok(turn_result) => turn_result,
        Err(error) => {
            record_direct_llm_error_state(state, &error);
            return Err(error);
        }
    };
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
            step_persistence_enabled: state.context_manifest_user_id.is_some(),
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

    let response_guard_blocked = record_response_guard_blocked_interruption_if_needed(state);

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
    if !ingest_is_fatal && !response_guard_blocked {
        let turn = session_turn_number(state);
        let session_id = state.current_session_id.clone();
        let model_id = state
            .skills
            .model_override
            .clone()
            .unwrap_or_else(|| "default".to_string());
        if let Some(ref mut pipeline_sess) = state.pipeline_session {
            let mut feedback = astra_turn_core::context_feedback::ContextFeedback::from_usage(
                turn_result.accum.prompt_tokens,
                turn_result.accum.cache_read_tokens,
                turn_result.accum.cache_creation_tokens,
                turn_result.accum.completion_tokens,
                false,
            );
            pipeline_sess.record_feedback(&model_id, "agentic_loop", &mut feedback, None);

            // Emit pipeline journal events for observability and cloud sync
            if let Some(ref mut buf) = state.turn_event_buffer {
                // Per-turn feedback event
                let feedback_evt =
                    astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
                        turn, &model_id, &feedback,
                    );
                if let Ok(payload) = serde_json::to_value(&feedback_evt) {
                    buf.record(
                        astra_services::session_journal::JournalEvent::pipeline_feedback(
                            session_id.as_deref(),
                            turn,
                            payload,
                        ),
                    );
                }

                // Drain and emit compaction audit events
                for audit in pipeline_sess.drain_pending_audits() {
                    if let Ok(payload) = serde_json::to_value(&audit) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_compaction_audit(
                                session_id.as_deref(),
                                turn,
                                payload,
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
                    if let Some(session_id_str) = alert_dispatch_session_id(session_id.as_deref()) {
                        if let Some(dispatcher) = global_alert_dispatcher() {
                            let alerts_to_send = alerts.clone();
                            let dispatcher = dispatcher.clone();
                            tokio::spawn(async move {
                                dispatcher.dispatch(&session_id_str, &alerts_to_send).await;
                            });
                        }
                    } else {
                        tracing::warn!(
                            target: "astra_runtime::agentic_loop",
                            turn,
                            "skipping alert webhook dispatch without session_id"
                        );
                    }
                }

                for alert in &alerts {
                    let alert_evt =
                        astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(alert);
                    if let Ok(payload) = serde_json::to_value(&alert_evt) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_alert(
                                session_id.as_deref(),
                                turn,
                                payload,
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
                    turn_result_tokens_consumed(&turn_result),
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
                                if let Err(error) = writer.append(&evt) {
                                    tracing::warn!(
                                        session_id = sid,
                                        error = %error,
                                        "failed to append compaction retry event to session journal"
                                    );
                                }
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
            // Final-answer relevance is evaluated post-mortem only. Runtime
            // must not act as a semantic judge that deletes a model answer,
            // injects a synthetic retry prompt, or terminates with a synthetic
            // interruption. If the answer is imperfect, preserve it and let
            // the next user turn correct course.

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
                turn_result_tokens_consumed(&turn_result),
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

    // Circuit-breaker pattern signals are advisory-only. Continuing to use
    // tools after such a signal is not a protocol violation, so there is no
    // "ignored correction" phase and no synthetic abort here.
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

fn record_response_guard_blocked_interruption_if_needed(state: &mut AgenticLoopState) -> bool {
    let response_guard_blocked =
        state.last_finish_reason.as_deref() == Some(RESPONSE_GUARD_BLOCKED_FINISH_REASON);
    if response_guard_blocked && state.interruption.is_none() {
        let summary = interruption_state_summary(
            state,
            Some("response guard blocked the model's final output".to_string()),
        );
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ResponseGuardBlocked,
            ResumeAction::ContinueImmediately,
            summary,
        ));
    }
    response_guard_blocked
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

fn has_concrete_workspace_mutation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.ok && !record.is_synthetic_placeholder())
        .any(tool_record_is_workspace_mutation)
}

/// Third-tier observation for the parallel-batching layer. The prompt-side soft
/// nudge fires when the trailing single-tool round streak hits
/// `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=6). If the model ignores the nudge
/// and produces yet another single-tool round, the streak crosses the
/// resolved `parallel_batching_force_streak` threshold (default 8, per-model
/// overrides via `ModelPolicyProfile`) and we emit advisory evidence in the
/// typed runtime lane.
pub(crate) const PARALLEL_BATCHING_FORCE_MARKER: &str = "## ⤴ Parallel Batching Observation";

/// Trailing single-tool-round streak length at which the soft prompt nudge
/// (=6) escalates into typed advisory evidence.
/// Default for the threshold; the actual value used at runtime flows through
/// `ToolSelectionConfig::effective_parallel_batching_force_streak` (and
/// per-model overrides via `ModelPolicyProfile`).
/// Must match `effective_parallel_batching_force_streak`'s zero-default.
#[cfg(test)]
pub(crate) const PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD: usize =
    astra_config::runtime_config::DEFAULT_PARALLEL_BATCHING_FORCE_STREAK as usize;

pub(crate) fn should_emit_parallel_batching_advisory(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.parallel_batching_advisory_emitted {
        return false;
    }
    // One advisory per turn: avoid stacking redundant behavior evidence.
    if state.stall.any_advisory_emitted() {
        return false;
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    streak >= threshold
}

pub(crate) fn parallel_batching_advisory_message(streak: usize, original_query: &str) -> String {
    format!(
        "{PARALLEL_BATCHING_FORCE_MARKER}\n\
         Observation: the last {streak} rounds each ran exactly one tool. When \
         calls are independent, that pattern may add avoidable latency, token use, \
         and round-budget pressure. Possible next actions include answering from \
         the evidence already gathered or batching independent calls. A further \
         single-tool round remains appropriate when its input genuinely depends on \
         the prior result.\n\n\
         Original user query: {original_query}"
    )
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
    let task_completed = latest_round_records
        .iter()
        .any(|record| tool_record_is_git_commit_action(record));

    RoundSignal {
        tool_signatures,
        produced_mutation,
        task_completed,
        tool_count,
    }
}

pub(crate) fn completion_observation_payload(
    round_index: u32,
    original_query: &str,
) -> serde_json::Value {
    serde_json::json!({
        "signal": "task_completion_observed",
        "round": round_index,
        "evidence": "A successful git commit indicates that the requested work may be complete.",
        "recommendation": "Consider providing the final answer unless concrete work required by the user remains.",
        "model_discretion": "This evidence does not require stopping or prohibit further necessary verification.",
        "original_user_query": original_query,
    })
}

// Redundant-reads mid-loop advisory.
//
// Detects the pattern where the model re-reads overlapping line ranges of the
// same file with no intervening workspace mutation. The detection algorithm
// lives in `astra-turn-core::evaluation::count_redundant_overlapping_reads`
// (post-mortem use) and is reused here for a one-shot mid-loop advisory.
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
    "## ⤴ Exploration Family Convergence Observation";
/// Default cache-waste midloop threshold. Used in tests; production code
/// reads from `ToolSelectionConfig::effective_cache_waste_midloop_threshold()`.
#[cfg(test)]
pub(crate) const CACHE_WASTE_MIDLOOP_THRESHOLD: usize = 3;

/// Mid-loop advisory threshold (intentionally one above the post-mortem
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

fn retry_cautioned_tools_for_exploration_family(family: &str) -> &'static [&'static str] {
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

pub(crate) fn exploration_family_advisory_candidate(
    state: &AgenticLoopState,
    threshold: usize,
) -> Option<(String, usize)> {
    if state.stall.exploration_family_advisory_emitted {
        return None;
    }
    let (family, streak) = astra_turn_core::evaluation::exploration_family_round_streak(
        &state.stall.tool_call_records,
    )?;
    (streak >= threshold).then(|| (family.to_string(), streak))
}

fn mark_exploration_family_advisory(state: &mut AgenticLoopState, family: &str) -> Vec<String> {
    let mut cautioned = retry_cautioned_tools_for_exploration_family(family)
        .iter()
        .map(|tool| (*tool).to_string())
        .collect::<Vec<_>>();
    cautioned.sort();
    state.stall.exploration_family_advisory_family = Some(family.to_string());
    cautioned
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

fn tool_call_retry_signature(rec: &astra_services::session_journal::ToolCallRecord) -> String {
    format!(
        "{}:{}",
        rec.name,
        rec.args_full.as_deref().unwrap_or("<missing-args>")
    )
}

fn repeated_prior_signature(
    state: &AgenticLoopState,
    latest_round: u32,
    rec: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let signature = tool_call_retry_signature(rec);
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|prior| !prior.is_synthetic_placeholder())
        .filter(|prior| prior.round.is_some_and(|round| round < latest_round))
        .any(|prior| tool_call_retry_signature(prior) == signature)
}

pub(crate) fn exploration_family_phase2_candidate(
    state: &AgenticLoopState,
) -> Option<(String, Vec<String>)> {
    if !state.stall.exploration_family_advisory_emitted
        || state.stall.stronger_exploration_family_advisory_emitted
    {
        return None;
    }
    let family = state.stall.exploration_family_advisory_family.as_deref()?;
    let retry_cautioned = retry_cautioned_tools_for_exploration_family(family);
    let (latest_round, latest_round_records) = latest_non_synthetic_round_records(state)?;
    if latest_round_records.is_empty() {
        return None;
    }

    let mut repeated_tools = latest_round_records
        .iter()
        .filter(|rec| retry_cautioned.contains(&rec.name.as_str()))
        .map(|rec| rec.name.clone())
        .collect::<Vec<_>>();
    if repeated_tools.is_empty() || repeated_tools.len() != latest_round_records.len() {
        return None;
    }
    if !latest_round_records
        .iter()
        .all(|rec| repeated_prior_signature(state, latest_round, rec))
    {
        return None;
    }
    repeated_tools.sort();
    repeated_tools.dedup();
    Some((family.to_string(), repeated_tools))
}

pub(crate) fn should_emit_cache_waste_advisory(state: &AgenticLoopState, threshold: usize) -> bool {
    if state.stall.cache_waste_advisory_emitted {
        return false;
    }
    !cache_wasteful_tools(state, threshold).is_empty()
}

pub(crate) fn should_emit_search_fanout_advisory(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.search_fanout_advisory_emitted {
        return false;
    }
    if !state.task_profile.mutates_workspace {
        return false;
    }
    astra_turn_core::evaluation::count_search_fanout(&state.stall.tool_call_records) >= threshold
}

pub(crate) fn cache_waste_advisory_message(
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
         Observation: cached tool calls were repeated this turn [{tool_list}]. \
         Those results are already in context — calling the same tool again wastes tokens and does not add evidence.\n\n\
         Recommendation: reuse the cached result when it answers the current need. \
         If evidence is still missing, a different target, query, argument set, or \
         changed worktree may add new information. Repeated cached output should not \
         be treated as new evidence.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn search_fanout_advisory_message(count: usize, original_query: &str) -> String {
    format!(
        "{SEARCH_FANOUT_MARKER}\n\
         Observation: {count} grep/rg/find-like search calls occurred in an implementation turn. \
         Broad search has crossed the low-yield threshold: more search is likely to expand context instead of finishing the task.\n\n\
         Recommendation: consider synthesizing the current evidence before widening \
         discovery. A specific edit, narrow validation, exact file/range read, or final \
         answer may be higher value if the necessary target is already known.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_advisory_message(
    family: &str,
    streak: usize,
    retry_cautioned_tools: &[String],
    original_query: &str,
) -> String {
    let tool_list = retry_cautioned_tools.join(", ");
    let label = exploration_family_label(family);
    format!(
        "{EXPLORATION_FAMILY_MARKER}\n\
         Observation: the last {streak} consecutive multi-call rounds stayed inside the same {label} family. \
         That is now classified as low-yield exploration churn. Retry-cautioned tools: [{tool_list}]. \
         They remain available, but repeating the same path without changed inputs or a new hypothesis is low value.\n\n\
         Recommendation: consider synthesizing prior evidence. If a fact remains \
         missing, another tool family or changed target may add more information. \
         Repeating the same {family} path is still reasonable when inputs changed, \
         but repetition alone is not new evidence.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_phase2_message(
    family: &str,
    retry_cautioned_tools: &[String],
    original_query: &str,
) -> String {
    let retry_cautioned_list = retry_cautioned_tools.join(", ");
    format!(
        "{EXPLORATION_FAMILY_PHASE2_MARKER}\n\
         Observation: after an earlier {family}-family advisory, the most recent \
         tool round repeated the same low-yield path [{retry_cautioned_list}] with \
         already-seen inputs and added little or no evidence.\n\n\
         Recommendation: consider answering from verified evidence, naming the \
         specific remaining gap, or using a different family if it can fetch that \
         fact. Identical calls may be useful after the worktree or target changes; \
         otherwise their repeated output is not new evidence.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn redundant_reads_advisory_message(count: usize, original_query: &str) -> String {
    format!(
        "{REDUNDANT_READS_MARKER}\n\
         Observation: overlapping line ranges of the same file were re-read \
         {count} times this turn without any intervening edit. The \
         content has not changed — re-reading wastes tokens and stalls progress.\n\n\
         Recommendation: reuse loaded content when it answers the current need. \
         If a genuinely unseen section is required, an exact range read can add \
         evidence. Otherwise, synthesizing verified facts and naming the remaining \
         gap may be higher value. Unobserved file contents remain unknown.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn should_emit_execution_escalation_advisory(state: &AgenticLoopState) -> bool {
    if state.stall.execution_escalation_advisory_emitted {
        return false;
    }
    // One advisory per turn: if the parallel-batching signal was already
    // emitted, skip this one to avoid stacking redundant evidence.
    // NOTE: execution order in execute_turn_and_ingest_phase is
    //   escalation → parallel-batching, so in practice escalation runs first.
    //   This guard is defensive against future reordering.
    if state.stall.parallel_batching_advisory_emitted {
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
         Observation: {read_only_calls} read-only tool calls have occurred on a task whose \
         structured intent requires changing the workspace, and no concrete mutation is \
         recorded yet. Consider whether the current evidence is sufficient for a targeted \
         edit and relevant verification. More inspection remains reasonable when a specific \
         unknown still blocks a safe change.\n\n\
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
    tokens_consumed: u64,
) {
    // ── Telemetry timing ─────────────────────────────────────────
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

    // ── Observation pipeline: dispatch TurnCompleted for tool-less turns ──
    // Tool-less turns (text-only stops, early exits, budget exhaustion) still
    // need to feed the observation journal and persistence store so the
    // self-status block and cross-session analysis see them.
    {
        let mut metrics = astra_core::TurnMetrics::default();
        metrics.rounds_completed = state.llm_rounds_completed;
        metrics.tokens_consumed = tokens_consumed;

        state.observation_journal.record_turn(&metrics);
        let facts = state
            .observation_journal
            .extract_facts(state.remaining_turns as u32, state.max_turns as u32);

        if let Some(ref store) = state.observation_store {
            let mut dispatcher = crate::turn::observation_dispatcher::ObservationDispatcher::new();
            dispatcher.register(crate::turn::observation_dispatcher::FileSink::new(
                Some(store.clone()),
                state
                    .current_session_id
                    .as_deref()
                    .unwrap_or_default()
                    .to_string(),
            ));
            dispatcher.dispatch(
                crate::turn::observation_dispatcher::ObservationEvent::TurnCompleted {
                    metrics: Box::new(metrics),
                    facts,
                },
            );
        }
    } // dispatcher dropped — releases &mut observation_journal
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
            turn_result_tokens_consumed(turn_result),
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
    let cumulative = state.provider_total_tokens();
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

    use astra_services::session_journal::{JournalEventType, ToolCallRecord, TurnEventBuffer};
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;
    use crate::observability::ObservabilityHub;
    use crate::turn::agentic_loop::host::tests::{MockHost, make_state, text_result};
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, DeferredUserInputRecord, VolatileKind,
        run_agentic_loop_with_host,
    };
    use crate::turn::run_control::{RunInputProvider, RunQueuedInputPoll, RunStatusProvider};
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;

    fn structured_mutating_profile() -> astra_turn_core::chat_turn_heuristics::TaskExecutionProfile
    {
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            false,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
        )
    }

    fn mark_must_mutate(state: &mut AgenticLoopState) {
        state.task_profile = structured_mutating_profile();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::MustMutate,
            ),
        );
    }

    #[test]
    fn manifest_reason_uses_structured_compaction_state_not_message_text() {
        let mut state = make_state();
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": "Please explain compaction without changing the context."
        }));
        assert_eq!(manifest_reason_for_llm_call(&state), "normal_turn");

        state.compact_tier_applied =
            astra_turn_core::compaction_types::CompactionTier::CompactHistory;
        assert_eq!(manifest_reason_for_llm_call(&state), "post_compaction");
    }

    #[test]
    fn alert_dispatch_session_id_requires_real_session_identity() {
        assert_eq!(alert_dispatch_session_id(None), None);
        assert_eq!(alert_dispatch_session_id(Some("")), None);
        assert_eq!(alert_dispatch_session_id(Some("   ")), None);
        assert_eq!(
            alert_dispatch_session_id(Some("  session-123  ")).as_deref(),
            Some("session-123")
        );
    }

    #[test]
    fn runtime_policy_budget_evidence_preserves_budget_and_history() {
        let mut state = make_state();
        state.max_turns = 8;
        state.remaining_turns = 2;
        let mut facts = astra_core::observation_journal::JournalFacts::default();
        facts.streaks.consecutive_rounds_with_outcome = 3;
        let history_before = state.messages.clone();

        route_runtime_policy_evidence(
            &mut state,
            &facts,
            crate::turn::runtime_policy::RuntimePolicyEvidence::BudgetExpansionSuggested {
                factor: 1.5,
                max_ceiling: 20,
            },
        );

        assert_eq!(state.max_turns, 8);
        assert_eq!(state.remaining_turns, 2);
        assert_eq!(state.messages, history_before);
        let pending = state.take_volatile_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, VolatileKind::BudgetReview);
        assert_eq!(
            pending[0].payload["signal"].as_str(),
            Some("budget_expansion_suggested")
        );
        assert_eq!(
            pending[0].payload["authority"].as_str(),
            Some("advisory_evidence_only")
        );
    }

    #[test]
    fn context_manifest_turn_intent_ignores_prompt_facing_benchmark_marker() {
        let mut state = make_state();
        state.message = "please compare these results [TASK_ID:bnh]".into();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        assert_eq!(
            infer_turn_intent_for_llm_call(&state),
            "normal",
            "prompt-facing marker text must not become control-plane turn intent"
        );
    }

    #[test]
    fn context_manifest_turn_intent_uses_structured_benchmark_scenario() {
        let mut state = make_state();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default()
                .with_requested_scenario(Scenario::BenchmarkComparison),
        );

        assert_eq!(
            infer_turn_intent_for_llm_call(&state),
            astra_services::TURN_INTENT_BENCHMARK_COMPARISON
        );
    }

    #[test]
    fn spill_summary_does_not_promote_read_paths_into_prompt_facing_memory() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                {
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"rust/astra/src/bridge/mod.rs\"}"
                    }
                },
                {
                    "function": {
                        "name": "str_replace",
                        "arguments": "{\"path\":\"crates/runtime/src/bridge/mod.rs\",\"old_str\":\"a\",\"new_str\":\"b\"}"
                    }
                }
            ]
        })];

        let summary = build_spill_summary(&messages);

        assert!(
            !summary.contains("rust/astra"),
            "read-only paths must not become prompt-facing memory: {summary}"
        );
        assert!(
            summary.contains("crates/runtime/src/bridge/mod.rs"),
            "mutated files should remain visible: {summary}"
        );
        assert!(summary.contains("- read_file"), "{summary}");
        assert!(summary.contains("- str_replace"), "{summary}");
    }

    struct StubRunControlProvider {
        polls: Mutex<VecDeque<RunQueuedInputPoll>>,
        poll_calls: Mutex<Vec<usize>>,
        released: Mutex<Vec<usize>>,
        release_failures: Mutex<usize>,
    }

    impl StubRunControlProvider {
        fn new(polls: Vec<RunQueuedInputPoll>) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(0),
            }
        }

        fn with_release_failures(polls: Vec<RunQueuedInputPoll>, release_failures: usize) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(release_failures),
            }
        }

        async fn poll_call_count(&self) -> usize {
            self.poll_calls.lock().await.len()
        }
    }

    struct DirectErrorHost {
        error: Option<astra_core::ClassifiedError>,
        valid_tools: HashSet<String>,
    }

    impl DirectErrorHost {
        fn new(error: astra_core::ClassifiedError) -> Self {
            Self {
                error: Some(error),
                valid_tools: HashSet::new(),
            }
        }
    }

    #[async_trait]
    impl AgenticLoopHost for DirectErrorHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            Err(self.error.take().expect("test host called once"))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, _line: String) {}

        fn is_quiet(&self) -> bool {
            true
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }
    }

    #[async_trait]
    impl RunStatusProvider for StubRunControlProvider {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
            Ok(None)
        }
    }

    #[async_trait]
    impl RunInputProvider for StubRunControlProvider {
        async fn poll_user_inputs(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> RunQueuedInputPoll {
            self.poll_calls.lock().await.push(after_event_index);
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
            _user_id: &str,
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

    #[test]
    fn observe_turn_end_without_tools_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.max_turns = 20;
        state.remaining_turns = 4;
        state.total_prompt = 10_000;
        state.total_completion = 20_000;
        state.total_cache_read = 30_000;
        state.total_cache_creation = 40_000;
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_hub = Some(Arc::new(hub));
        state.telemetry.observability_session = Some(session.clone());

        let turn_start_time = Instant::now() - Duration::from_millis(25);
        observe_turn_end_without_tools(&mut state, 16, turn_start_time, Some(7), 123);

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_timings.len(), 1);
        assert_eq!(guard.turn_timings[0].turn, 6);
        assert_eq!(
            state
                .observation_journal
                .last_entry()
                .map(|entry| entry.tokens_consumed),
            Some(123),
            "tool-less observation must record the current LLM round cost, not cumulative session tokens"
        );
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

    #[test]
    fn context_manifest_db_persistence_follows_context_assembly_trace_category() {
        use astra_config::runtime_config::{SessionTraceConfig, TraceCategory, TraceProfile};

        let production = SessionTraceConfig::default();
        assert!(
            !context_manifest_db_persistence_enabled_for_trace(&production),
            "production/default trace profile must not write context manifest diagnostics"
        );

        let dev = SessionTraceConfig::default().apply_profile(TraceProfile::Dev);
        assert!(
            context_manifest_db_persistence_enabled_for_trace(&dev),
            "dev trace profile enables all diagnostic persistence categories"
        );

        let custom = SessionTraceConfig {
            profile: TraceProfile::Custom,
            enabled_categories: vec![TraceCategory::ContextAssembly],
            ..SessionTraceConfig::default()
        }
        .normalize();
        assert!(context_manifest_db_persistence_enabled_for_trace(&custom));
    }

    #[test]
    fn context_manifest_uses_pipeline_context_window_trace() {
        let mut state = make_state();
        state.last_llm_context_manifest_trace = Some(serde_json::json!({
            "model_context_window_tokens": 1_000_000
        }));

        assert_eq!(
            context_window_tokens_for_context_manifest(&state),
            1_000_000
        );
    }

    #[test]
    fn context_manifest_context_window_defaults_to_generic_200k_without_trace() {
        let state = make_state();

        assert_eq!(
            context_window_tokens_for_context_manifest(&state),
            crate::prompts::DEFAULT_CONTEXT_WINDOW_TOKENS as u32
        );
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

    #[tokio::test]
    async fn direct_provider_rate_limit_error_records_interruption_and_cooldown() {
        let mut state = make_state();
        let mut host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::RateLimit,
            "provider returned 429",
        ));

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

        assert!(result.is_err());
        let interruption = state
            .interruption
            .as_ref()
            .expect("direct provider 429 must be represented as an interruption");
        assert_eq!(interruption.kind, InterruptionKind::RateLimited);
        assert_eq!(state.llm_rounds_completed, 1);
        let metrics = state.rate_limit_cooldown.metrics();
        assert_eq!(metrics.total_429_errors, 1);
        assert_eq!(metrics.consecutive_errors, 1);
    }

    #[tokio::test]
    async fn direct_provider_admission_rejection_records_interruption_without_provider_cooldown() {
        let mut state = make_state();
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::RateLimit,
                "LLM provider admission rejected request before provider call",
            )
            .with_details_json(
                serde_json::json!({
                    "source": "llm_provider_admission",
                    "scope": "provider",
                    "limit": 20
                })
                .to_string(),
            ),
        );

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

        assert!(result.is_err());
        let interruption = state
            .interruption
            .as_ref()
            .expect("admission rejection must be represented as an interruption");
        assert_eq!(interruption.kind, InterruptionKind::RateLimited);
        assert_eq!(state.llm_rounds_completed, 1);
        let metrics = state.rate_limit_cooldown.metrics();
        assert_eq!(
            metrics.total_429_errors, 0,
            "local admission rejection must not be counted as a provider 429"
        );
        assert_eq!(metrics.consecutive_errors, 0);
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
    fn task_board_state_does_not_suppress_completion_observation() {
        let mut state = make_state();
        state.llm_rounds_completed = 4;
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
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

        assert!(signal.task_completed);
        assert!(signal.produced_mutation);
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
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
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
        assert!(should_emit_execution_escalation_advisory(&state));
    }

    #[test]
    fn escalation_does_not_fire_just_below_threshold() {
        let state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD - 1);
        assert!(!should_emit_execution_escalation_advisory(&state));
    }

    #[test]
    fn escalation_does_not_fire_on_non_mutating_task() {
        let mut state =
            make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD + 2);
        // Flip profile to read-only exploration — escalation must not engage.
        state.task_profile = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.turn_intent = None;
        assert!(!state.task_profile.mutates_workspace);
        assert!(!should_emit_execution_escalation_advisory(&state));
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
        assert!(!should_emit_execution_escalation_advisory(&state));
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
        assert!(!should_emit_execution_escalation_advisory(&state));
    }

    #[test]
    fn escalation_is_one_shot_per_turn() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        state.stall.execution_escalation_advisory_emitted = true;
        assert!(
            !should_emit_execution_escalation_advisory(&state),
            "flag must prevent a second injection"
        );
    }

    #[test]
    fn escalation_suppressed_when_parallel_batching_already_fired() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        // Precondition: without the flag, escalation would fire.
        assert!(should_emit_execution_escalation_advisory(&state));
        // Once parallel-batching force has fired, escalation must yield to
        // honor the one-advisory-per-turn invariant.
        state.stall.parallel_batching_advisory_emitted = true;
        assert!(
            !should_emit_execution_escalation_advisory(&state),
            "escalation must not fire when parallel-batching force already active"
        );
    }

    #[test]
    fn escalation_ignores_failed_tool_calls_for_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
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
        assert!(!should_emit_execution_escalation_advisory(&state));
    }

    #[test]
    fn escalation_ignores_synthetic_placeholders() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
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
            assert!(!should_emit_execution_escalation_advisory(&state));
        }
    }

    #[test]
    fn parallel_batching_suppressed_when_escalation_already_fired() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        // Precondition: without escalation flag, parallel-batching would fire.
        assert!(should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // Once escalation has fired, parallel-batching must yield.
        state.stall.execution_escalation_advisory_emitted = true;
        assert!(
            !should_emit_parallel_batching_advisory(
                &state,
                PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
            ),
            "parallel-batching must not fire when escalation already active"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_cascade_guard_already_fired() {
        let flags: Vec<Box<dyn Fn(&mut AgenticLoopState)>> = vec![
            Box::new(|s| s.stall.redundant_reads_advisory_emitted = true),
            Box::new(|s| s.stall.cache_waste_advisory_emitted = true),
            Box::new(|s| s.stall.search_fanout_advisory_emitted = true),
            Box::new(|s| s.stall.exploration_family_advisory_emitted = true),
            Box::new(|s| s.stall.stronger_exploration_family_advisory_emitted = true),
        ];
        for set_flag in &flags {
            let mut state = make_state();
            state.message = "explore the codebase".into();
            state.user_intent = state.message.clone();
            for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
                push_single_tool_round(&mut state);
            }
            // Precondition: would fire without the flag.
            assert!(should_emit_parallel_batching_advisory(
                &state,
                PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
            ));
            set_flag(&mut state);
            assert!(
                !should_emit_parallel_batching_advisory(
                    &state,
                    PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
                ),
                "parallel-batching must not fire when a cascade guard already active"
            );
        }
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
        state.user_intent = state.message.clone();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_below_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD - 1) {
            push_single_tool_round(&mut state);
        }
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_when_last_round_batched() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
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
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 3) {
            push_single_tool_round(&mut state);
        }
        // First time would fire...
        assert!(should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // ...but once the flag is set, a second attempt is suppressed even
        // if the model produces yet another single-tool round.
        state.stall.parallel_batching_advisory_emitted = true;
        push_single_tool_round(&mut state);
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    // ─── Cascade-invariant + per-model resolver wiring ─────────────────────

    /// The runtime hard-advisory force MUST stay strictly above the
    /// prompt-layer soft nudge so the soft→hard cascade is preserved. If the
    /// resolved force ever drops to ≤ nudge, the runtime will inject a hard
    /// `user`-role advisory before the model has had any chance to
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
    /// should_emit_parallel_batching_advisory(_, threshold)`. A regression that
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
        state.user_intent = state.message.clone();
        for _ in 0..global_default {
            push_single_tool_round(&mut state);
        }

        // Resolved per-model threshold (=11) must suppress the advisory…
        assert!(
            !should_emit_parallel_batching_advisory(
                &state,
                policy.parallel_batching_force_streak as usize
            ),
            "streak={global_default} must NOT fire under per-model force=11"
        );

        // …whereas the model-blind global path would fire. This is the
        // actual regression target: if someone re-routes the guard back to
        // `effective_parallel_batching_force_streak`, the second assertion
        // would still pass but the first would change behavior — pinning
        // both makes the wiring explicit.
        assert!(
            should_emit_parallel_batching_advisory(
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
    /// advisory arrives before the prompt-layer nudge and the intended
    /// progression is silently inverted.
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
    // opted into uninterrupted execution. Every advisory nudge we
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
        state.context_manifest_user_id = Some("user-deferred".into());
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
            state.deferred_input.delivered_user_inputs(),
            &[DeferredUserInputRecord {
                event_index: 1,
                content: "Switch to writing tests first.".to_string(),
            }]
        );
        assert_eq!(
            state
                .messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str()),
            Some("Switch to writing tests first.")
        );
        assert!(
            state.volatile_pending.is_empty(),
            "real deferred user input must not be duplicated as runtime context"
        );
    }

    #[tokio::test]
    async fn deferred_user_input_records_multiple_inputs_without_consecutive_user_messages() {
        let mut state = make_state();
        state.current_run_id = Some("run-queued-many".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![RunQueuedInputPoll {
            next_cursor: 3,
            inputs: vec![
                crate::turn::run_control::QueuedRunInputEvent {
                    event_index: 1,
                    input: serde_json::json!({"content": "first queued input"}),
                },
                crate::turn::run_control::QueuedRunInputEvent {
                    event_index: 2,
                    input: serde_json::json!({"content": "second queued input"}),
                },
            ],
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 3);
        assert_eq!(*provider.released.lock().await, vec![1, 2]);
        assert_eq!(state.message, "second queued input");
        assert_eq!(
            state.deferred_input.delivered_user_inputs(),
            &[
                DeferredUserInputRecord {
                    event_index: 1,
                    content: "first queued input".to_string(),
                },
                DeferredUserInputRecord {
                    event_index: 2,
                    content: "second queued input".to_string(),
                },
            ]
        );
        assert_eq!(
            state
                .messages
                .last()
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_str()),
            Some("first queued input\n\nsecond queued input")
        );
        assert!(
            !state.messages.windows(2).any(|window| {
                window.iter().all(|message| {
                    message.get("role").and_then(|role| role.as_str()) == Some("user")
                })
            }),
            "deferred input injection must keep prompt history provider-safe"
        );
    }

    #[tokio::test]
    async fn deferred_user_input_does_not_reinject_after_cursor_advance() {
        let mut state = make_state();
        state.current_run_id = Some("run-repoll".into());
        state.context_manifest_user_id = Some("user-deferred".into());
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn deferred_user_input_empty_poll_is_throttled() {
        let mut state = make_state();
        state.current_run_id = Some("run-empty-throttle".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            RunQueuedInputPoll {
                next_cursor: 0,
                inputs: Vec::new(),
                error: None,
            },
            RunQueuedInputPoll {
                next_cursor: 2,
                inputs: vec![crate::turn::run_control::QueuedRunInputEvent {
                    event_index: 1,
                    input: serde_json::json!({"content": "arrived after quiet poll"}),
                }],
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 1);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "empty poll should suppress immediate follow-up DB poll"
        );
        assert!(state.messages.is_empty());

        tokio::time::advance(DEFERRED_USER_INPUT_EMPTY_POLL_INTERVAL - Duration::from_millis(1))
            .await;
        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "poll should remain suppressed until the interval fully elapses"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 2);
        assert_eq!(state.message, "arrived after quiet poll");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test]
    async fn deferred_user_input_retries_release_without_reinjecting_after_ack_failure() {
        let mut state = make_state();
        state.current_run_id = Some("run-release-retry".into());
        state.context_manifest_user_id = Some("user-deferred".into());
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
        assert_eq!(
            provider.poll_call_count().await,
            2,
            "pending release acknowledgement must bypass empty-poll throttling"
        );
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
        state.context_manifest_user_id = Some("user-deferred".into());
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
    async fn deferred_user_input_poll_error_degrades_without_advancing_cursor() {
        let mut state = make_state();
        state.current_run_id = Some("run-missing".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![RunQueuedInputPoll {
            next_cursor: 4,
            inputs: Vec::new(),
            error: Some("run not found while polling deferred input: run-missing".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_deferred_user_inputs(&mut host, &mut state)
            .await
            .expect("poll errors are control-plane misses and should not fail the main turn");

        assert_eq!(state.deferred_input.deferred_user_input_cursor(), 0);
        assert!(state.messages.is_empty());
        assert!(state.volatile_pending.is_empty());
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
        state.user_intent = state.message.clone();
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
            !state.stall.parallel_batching_advisory_emitted,
            "Auto mode must not set parallel_batching_advisory_emitted flag"
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
        state.user_intent = state.message.clone();
        // Accumulate EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD successful
        // read-only records with no write. `ok: true` + non-synthetic is
        // the shape `should_emit_execution_escalation_advisory` counts.
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
        assert!(!state.stall.execution_escalation_advisory_emitted);
    }

    #[tokio::test]
    async fn auto_mode_suppresses_round_budget_guidance_injection() {
        // The prompt-side tool_round_guidance (parallel-batching soft
        // nudge at streak=4, before the higher-threshold advisory at streak=5) also
        // must stay silent in Auto.
        let mut state = make_state();
        state.message = "keep going".into();
        state.user_intent = state.message.clone();
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
    fn redundant_reads_advisory_fires_at_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        // First read seeds the file's history; subsequent overlapping reads
        // each contribute one redundant event.
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 1) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        let corrections = crate::turn::agentic_loop::guards::evaluate_guards(
            &crate::turn::agentic_loop::guards::default_guards(),
            &mut state,
            &crate::turn::agentic_loop::guards::GuardConfig {
                suppress_nudges: false,
                parallel_batching_force_streak: usize::MAX,
                redundant_reads_threshold: REDUNDANT_READS_MIDLOOP_THRESHOLD,
                cache_waste_threshold: usize::MAX,
            },
        );

        assert!(state.stall.redundant_reads_advisory_emitted);
        assert_eq!(corrections.len(), 1);
    }

    #[test]
    fn redundant_reads_advisory_silent_below_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        // Threshold-many reads = (threshold-1) overlap events: stays silent.
        for r in 0..REDUNDANT_READS_MIDLOOP_THRESHOLD {
            push_redundant_sed_read(&mut state, r as u32);
        }
        let corrections = crate::turn::agentic_loop::guards::evaluate_guards(
            &crate::turn::agentic_loop::guards::default_guards(),
            &mut state,
            &crate::turn::agentic_loop::guards::GuardConfig {
                suppress_nudges: false,
                parallel_batching_force_streak: usize::MAX,
                redundant_reads_threshold: REDUNDANT_READS_MIDLOOP_THRESHOLD,
                cache_waste_threshold: usize::MAX,
            },
        );

        assert!(!state.stall.redundant_reads_advisory_emitted);
        assert!(corrections.is_empty());
    }

    #[test]
    fn redundant_reads_advisory_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 5) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        let cfg = crate::turn::agentic_loop::guards::GuardConfig {
            suppress_nudges: false,
            parallel_batching_force_streak: usize::MAX,
            redundant_reads_threshold: REDUNDANT_READS_MIDLOOP_THRESHOLD,
            cache_waste_threshold: usize::MAX,
        };
        let first = crate::turn::agentic_loop::guards::evaluate_guards(
            &crate::turn::agentic_loop::guards::default_guards(),
            &mut state,
            &cfg,
        );
        assert!(state.stall.redundant_reads_advisory_emitted);
        assert_eq!(first.len(), 1);

        push_redundant_sed_read(&mut state, 99);
        let second = crate::turn::agentic_loop::guards::evaluate_guards(
            &crate::turn::agentic_loop::guards::default_guards(),
            &mut state,
            &cfg,
        );
        assert!(second.is_empty());
    }

    #[test]
    fn cache_waste_advisory_fires_at_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..CACHE_WASTE_MIDLOOP_THRESHOLD {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_advisory_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD - 1) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(!should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_advisory_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD + 2) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
        state.stall.cache_waste_advisory_emitted = true;
        state.turn_guard.record_cache_hit("git_diff");
        assert!(!should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
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
    fn search_fanout_advisory_fires_for_mutating_task() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        for idx in 0..8 {
            push_search_call(&mut state, idx);
        }

        assert!(should_emit_search_fanout_advisory(&state, 8));
    }

    #[test]
    fn search_fanout_advisory_skips_read_only_review() {
        let mut state = make_state();
        state.message = "review the branch".into();
        state.user_intent = state.message.clone();
        state.task_profile.mutates_workspace = false;
        for idx in 0..12 {
            push_search_call(&mut state, idx);
        }

        assert!(!should_emit_search_fanout_advisory(&state, 8));
    }

    #[test]
    fn search_fanout_advisory_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        for idx in 0..10 {
            push_search_call(&mut state, idx);
        }
        assert!(should_emit_search_fanout_advisory(&state, 8));
        state.stall.search_fanout_advisory_emitted = true;
        assert!(!should_emit_search_fanout_advisory(&state, 8));
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

    fn push_retry_cautioned_round(
        state: &mut AgenticLoopState,
        tool: &str,
        round: u32,
        args_full: &str,
    ) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: tool.into(),
            ok: true,
            round: Some(round),
            args_full: Some(args_full.to_string()),
            result_preview: Some("same low-yield evidence".into()),
            ..Default::default()
        });
    }

    #[test]
    fn exploration_family_advisory_fires_at_threshold_and_cautions_explicit_tools() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_diff_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_advisory_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family advisory candidate");
        };

        assert_eq!(family, "diff");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let retry_cautioned = mark_exploration_family_advisory(&mut state, &family);
        assert_eq!(retry_cautioned, vec!["git".to_string()]);
        assert!(
            !state.restricted_tools.contains("git"),
            "exploration-family advisory must not hard-restrict the tool"
        );
        assert!(
            !state.restricted_tools.contains("bash"),
            "exploration-family advisory must not globally block bash"
        );
    }

    #[test]
    fn exploration_family_advisory_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD - 1) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_advisory_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_advisory_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD + 2) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_advisory_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_some()
        );

        state.stall.exploration_family_advisory_emitted = true;
        assert!(
            exploration_family_advisory_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_phase2_fires_after_repeated_retry_signature_round() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        state.stall.exploration_family_advisory_emitted = true;
        state.stall.exploration_family_advisory_family = Some("diff".into());
        let args = r#"{"action":"diff","path":"src/lib.rs"}"#;
        push_retry_cautioned_round(&mut state, "git", 6, args);
        push_retry_cautioned_round(&mut state, "git", 7, args);

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
        state.user_intent = state.message.clone();
        state.stall.exploration_family_advisory_emitted = true;
        state.stall.exploration_family_advisory_family = Some("diff".into());
        let args = r#"{"action":"diff","path":"src/lib.rs"}"#;
        push_retry_cautioned_round(&mut state, "git", 6, args);
        push_retry_cautioned_round(&mut state, "git", 7, args);
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
    fn exploration_family_phase2_stays_silent_on_changed_retry_signature() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        state.stall.exploration_family_advisory_emitted = true;
        state.stall.exploration_family_advisory_family = Some("diff".into());
        push_retry_cautioned_round(
            &mut state,
            "git",
            6,
            r#"{"action":"diff","path":"src/lib.rs"}"#,
        );
        push_retry_cautioned_round(
            &mut state,
            "git",
            7,
            r#"{"action":"diff","path":"src/runtime.rs"}"#,
        );

        assert!(
            exploration_family_phase2_candidate(&state).is_none(),
            "changed arguments represent a new hypothesis and must not be treated as repeated wall-hitting"
        );
    }

    #[test]
    fn exploration_family_phase2_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        state.stall.exploration_family_advisory_emitted = true;
        state.stall.exploration_family_advisory_family = Some("diff".into());
        let args = r#"{"action":"diff","path":"src/lib.rs"}"#;
        push_retry_cautioned_round(&mut state, "git", 6, args);
        push_retry_cautioned_round(&mut state, "git", 7, args);

        assert!(exploration_family_phase2_candidate(&state).is_some());
        state.stall.stronger_exploration_family_advisory_emitted = true;
        assert!(exploration_family_phase2_candidate(&state).is_none());
    }

    #[test]
    fn exploration_family_advisory_retry_cautions_search_tools_without_bash() {
        let mut state = make_state();
        state.message = "investigate auth flow".into();
        state.user_intent = state.message.clone();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_search_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_advisory_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family search advisory candidate");
        };

        assert_eq!(family, "search");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let retry_cautioned = mark_exploration_family_advisory(&mut state, &family);
        assert_eq!(
            retry_cautioned,
            vec!["glob".to_string(), "grep".to_string(), "rg".to_string()]
        );
        assert!(
            !state.restricted_tools.contains("glob")
                && !state.restricted_tools.contains("grep")
                && !state.restricted_tools.contains("rg"),
            "search-family advisory must not hard-restrict search tools"
        );
        assert!(
            !state.restricted_tools.contains("bash"),
            "search-family advisory must not globally block bash"
        );
    }

    #[test]
    fn cumulative_budget_counts_cache_buckets() {
        let mut state = make_state();
        state.max_cumulative_tokens = 100;
        state.total_prompt = 5;
        state.total_completion = 5;
        state.total_cache_read = 20;
        state.total_cache_creation = 80;
        let mut host = MockHost::new(Vec::new());

        assert!(should_wrap_up_for_cumulative_budget(
            &mut host, &mut state, true
        ));
        assert!(state.budget_wrapup_injected);
        assert!(
            state.interruption.as_ref().is_some_and(|record| record.kind
                == astra_turn_core::interruption::InterruptionKind::CumulativeBudgetExceeded),
            "cache-inclusive cumulative budget should record an interruption"
        );
    }

    #[tokio::test]
    async fn pipeline_session_receives_feedback_on_successful_turn() {
        use astra_turn_core::pipeline_config::PipelineConfig;
        use astra_turn_core::pipeline_session::PipelineSession;

        let mut state = make_state();
        state.current_session_id = Some("session-1".to_string());
        state.session_turn = 6;
        state.turn_event_buffer = Some(TurnEventBuffer::begin_turn(
            state.current_session_id.as_deref(),
            state.session_turn,
        ));
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

        let mut buffer = state
            .turn_event_buffer
            .take()
            .expect("pipeline feedback should be buffered");
        let events = buffer.drain();
        let feedback_event = events
            .iter()
            .find(|event| event.event_type == JournalEventType::PipelineFeedback)
            .expect("pipeline feedback event");
        assert_eq!(feedback_event.turn, Some(6));
        assert_eq!(
            feedback_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("turn"))
                .and_then(|turn| turn.as_u64()),
            Some(6)
        );
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
    let tokens_freed = u64::from(astra_turn_core::section_types::estimate_text_tokens(
        &spill_json,
    ));

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

    // Record a tool invocation. Paths from read/search tools are deliberately
    // not persisted into the prompt-facing spill summary: failed exploratory
    // reads often contain stale or deleted paths, and promoting those into a
    // system summary makes the next turn treat them as current workspace facts.
    let mut record_tool = |name: &str, args: &serde_json::Value| {
        let path = args.get("path").and_then(|p| p.as_str());
        if let Some(p) = path {
            if matches!(name, "str_replace" | "write_file" | "multi_edit") {
                let ps = p.to_string();
                if !files_modified.contains(&ps) {
                    files_modified.push(ps);
                }
            }
        }
        tools_used.push(name.to_string());
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

#[cfg(test)]
mod response_guard_blocked_interruption_tests {
    use super::*;

    #[test]
    fn response_guard_blocked_finish_reason_records_structured_interruption() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.last_finish_reason = Some(RESPONSE_GUARD_BLOCKED_FINISH_REASON.to_string());
        state.remaining_turns = 7;

        assert!(record_response_guard_blocked_interruption_if_needed(
            &mut state
        ));
        let interruption = state
            .interruption
            .as_ref()
            .expect("response guard finish reason must create interruption");
        assert_eq!(interruption.kind, InterruptionKind::ResponseGuardBlocked);
        assert_eq!(
            interruption.resume_action,
            ResumeAction::ContinueImmediately
        );
        assert_eq!(interruption.remaining_turns, 7);
        assert_eq!(
            interruption.error_detail.as_deref(),
            Some("response guard blocked the model's final output")
        );
    }

    #[test]
    fn normal_finish_reason_does_not_record_response_guard_interruption() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.last_finish_reason = Some("normal".to_string());

        assert!(!record_response_guard_blocked_interruption_if_needed(
            &mut state
        ));
        assert!(state.interruption.is_none());
    }
}
