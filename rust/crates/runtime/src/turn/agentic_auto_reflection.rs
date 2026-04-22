use std::sync::Arc;

use astra_services::SelfSurfaceService;
use astra_services::self_surface::{
    EventPreview, EvolutionRecord, GoalSurface, HealthSurface, LocalSelfSurfaceService,
    PersistentSelfSnapshot, SelfSurfaceDimension, SelfSurfaceResponse, ToolFailureView,
    ToolHealthView, VerificationEventView, VerificationSurface,
};

use crate::str_preview::truncate_str;
use crate::turn::agentic_headless_round::HeadlessStderrStyle;

use super::agentic_adaptive_tuning::{
    MAX_RECENT_TACTICAL_ACTIONS, record_new_evolution_promotion_events,
    snapshot_evolution_promotion_ids,
};
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopState, HostReflectionRequest, HostReflectionResult,
};

#[allow(dead_code)]
pub(crate) const AUTO_REFLECTION_SIGNAL_THRESHOLD: usize = 3;
const AUTO_REFLECTION_MAX_OUTPUT_TOKENS: usize = 1200;
const AUTO_REFLECTION_TOOL_WINDOW: usize = 24;
const AUTO_REFLECTION_TOOL_STAT_LIMIT: usize = 8;
const AUTO_REFLECTION_SELF_EVIDENCE_JOURNAL_LIMIT: usize = 12;

fn build_auto_reflection_tool_stats(
    state: &AgenticLoopState,
) -> Vec<crate::liquid::reflection::ToolStat> {
    let start = state
        .stall
        .tool_call_records
        .len()
        .saturating_sub(AUTO_REFLECTION_TOOL_WINDOW);
    crate::liquid::reflection::ToolStat::summarize_records(
        &state.stall.tool_call_records[start..],
        AUTO_REFLECTION_TOOL_STAT_LIMIT,
    )
}

fn build_auto_reflection_experiment_summary(
    _state: &AgenticLoopState,
) -> Option<crate::liquid::reflection::ExperimentSummary> {
    None
}

fn reflection_goal_summary_from_surface(
    goal: GoalSurface,
) -> Option<crate::liquid::reflection::GoalSummary> {
    Some(crate::liquid::reflection::GoalSummary {
        effective_goal: goal.goal?,
        goal_source: goal.goal_source,
        tracking_status: goal.tracking_status,
        progress_summary: goal.progress.map(|progress| progress.summary),
    })
}

fn reflection_verification_summary_from_surface(
    verification: VerificationSurface,
) -> crate::liquid::reflection::VerificationSummary {
    crate::liquid::reflection::VerificationSummary {
        ok: verification.ok,
        acceptance_ok: verification.acceptance_ok,
        objective_ok: verification.objective_ok,
        summary: verification.summary,
        pending_blockers: verification.objective.pending_blockers,
        latest_verification: verification
            .objective
            .latest_verification
            .map(|event| event.summary),
    }
}

fn reflection_health_summary_from_surface(
    health: HealthSurface,
) -> Option<crate::liquid::reflection::HealthSummary> {
    let risk_flags = health.risk_flags.into_iter().take(4).collect::<Vec<_>>();
    let blocked_tools = health.blocked_tools.into_iter().take(4).collect::<Vec<_>>();
    let hotspots = health
        .tool_hotspots
        .into_iter()
        .take(3)
        .map(reflection_tool_hotspot_summary)
        .collect::<Vec<_>>();
    let recent_failures = health
        .recent_failures
        .into_iter()
        .take(3)
        .map(reflection_tool_failure_summary)
        .collect::<Vec<_>>();
    if risk_flags.is_empty()
        && blocked_tools.is_empty()
        && hotspots.is_empty()
        && recent_failures.is_empty()
    {
        return None;
    }
    Some(crate::liquid::reflection::HealthSummary {
        risk_flags,
        blocked_tools,
        hotspots,
        recent_failures,
    })
}

fn reflection_tool_hotspot_summary(tool: ToolHealthView) -> String {
    let mut parts = vec![format!(
        "{} success={:.0}%",
        tool.name,
        tool.success_rate * 100.0
    )];
    if tool.deprioritized {
        parts.push("deprioritized".into());
    }
    if tool.consecutive_failures > 0 {
        parts.push(format!(
            "consecutive_failures={}",
            tool.consecutive_failures
        ));
    }
    if tool.rehabilitation_count > 0 {
        parts.push(format!("rehab={}", tool.rehabilitation_count));
    }
    parts.join(", ")
}

fn reflection_tool_failure_summary(failure: ToolFailureView) -> String {
    let mut detail = match failure.turn {
        Some(turn) => format!("turn {turn} {}", failure.tool),
        None => failure.tool,
    };
    if let Some(error) = failure.error {
        detail.push_str(" — ");
        detail.push_str(&truncate_str(&error, 80));
    }
    detail
}

fn compact_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::String(inner) => truncate_str(inner, 80),
        _ => truncate_str(&value.to_string(), 80),
    }
}

fn reflection_goal_event_summary(
    event: &EventPreview,
) -> Option<crate::liquid::reflection::ReflectionEventSummary> {
    if event.event_type != "goal_steered" {
        return None;
    }
    let metadata = event.metadata.as_ref();
    let source = metadata
        .and_then(|meta| meta.get("source"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("goal_steered");
    let previous_goal = metadata
        .and_then(|meta| meta.get("previous_goal"))
        .and_then(serde_json::Value::as_str)
        .filter(|goal| !goal.is_empty());
    let new_goal = metadata
        .and_then(|meta| meta.get("new_goal"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("goal updated");
    let mut detail = match previous_goal {
        Some(previous_goal) => format!("{source}: {previous_goal} -> {new_goal}"),
        None => format!("{source}: {new_goal}"),
    };
    if let Some(extra) = metadata
        .and_then(|meta| meta.get("detail"))
        .filter(|value| !value.is_null())
    {
        detail.push_str(&format!(" ({})", compact_json_value(extra)));
    }
    Some(crate::liquid::reflection::ReflectionEventSummary {
        kind: "GoalSteered".into(),
        turn: event.turn,
        detail,
    })
}

fn reflection_verification_event_summary(
    event: &VerificationEventView,
) -> crate::liquid::reflection::ReflectionEventSummary {
    let outcome = match event.passed {
        Some(true) => "passed",
        Some(false) => "failed",
        None => "recorded",
    };
    let mut detail = outcome.to_string();
    if let Some(scope) = event.scope.as_deref() {
        detail.push(' ');
        detail.push_str(scope);
    }
    if let Some(target) = event.target.as_deref() {
        detail.push(' ');
        detail.push_str(target);
    }
    detail.push_str(" — ");
    detail.push_str(&truncate_str(&event.summary, 120));
    crate::liquid::reflection::ReflectionEventSummary {
        kind: "Verification".into(),
        turn: event.turn,
        detail,
    }
}

fn reflection_recent_evaluation_events(
    goal: Option<&GoalSurface>,
    verification: Option<&VerificationSurface>,
) -> Vec<crate::liquid::reflection::ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let mut events = Vec::new();
    if let Some(goal) = goal {
        events.extend(
            goal.recent_goal_events
                .iter()
                .filter_map(reflection_goal_event_summary),
        );
    }
    if let Some(verification) = verification {
        events.extend(
            verification
                .objective
                .recent_verifications
                .iter()
                .map(reflection_verification_event_summary),
        );
    }
    events.sort_by(|a, b| {
        b.turn
            .unwrap_or_default()
            .cmp(&a.turn.unwrap_or_default())
            .then_with(|| a.kind.cmp(&b.kind))
    });
    events.truncate(REFLECTION_EVENT_LIMIT);
    events
}

fn reflection_recent_adaptations(
    snapshot: Option<&PersistentSelfSnapshot>,
) -> Vec<crate::liquid::reflection::ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    snapshot
        .evolution
        .records
        .iter()
        .filter_map(reflection_adaptation_summary)
        .take(REFLECTION_EVENT_LIMIT)
        .collect()
}

fn reflection_adaptation_summary(
    record: &EvolutionRecord,
) -> Option<crate::liquid::reflection::ReflectionEventSummary> {
    if !is_reflection_adaptation_record(record) {
        return None;
    }
    Some(crate::liquid::reflection::ReflectionEventSummary {
        kind: reflection_kind_label(&record.kind),
        turn: record.turn,
        detail: format!("{} — {}", record.status, truncate_str(&record.summary, 120)),
    })
}

fn reflection_recent_adaptation_outcomes(
    snapshot: Option<&PersistentSelfSnapshot>,
) -> Vec<crate::liquid::reflection::ReflectionEventSummary> {
    const REFLECTION_EVENT_LIMIT: usize = 4;

    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    let mut previous_adaptation_index = None;
    for (index, record) in snapshot.evolution.records.iter().enumerate() {
        if !is_reflection_adaptation_record(record) {
            continue;
        }
        let start = previous_adaptation_index.map_or(0, |previous| previous + 1);
        if let Some(outcome) = snapshot.evolution.records[start..index]
            .iter()
            .find(|candidate| is_reflection_adaptation_outcome_record(candidate))
        {
            outcomes.push(reflection_adaptation_outcome_summary(record, outcome));
        }
        previous_adaptation_index = Some(index);
        if outcomes.len() >= REFLECTION_EVENT_LIMIT {
            break;
        }
    }
    outcomes
}

fn reflection_adaptation_outcome_summary(
    adaptation: &EvolutionRecord,
    outcome: &EvolutionRecord,
) -> crate::liquid::reflection::ReflectionEventSummary {
    let detail = match adaptation.turn {
        Some(turn) => format!(
            "after {} turn {} — {}",
            reflection_kind_label(&adaptation.kind),
            turn,
            truncate_str(&outcome.summary, 120)
        ),
        None => format!(
            "after {} — {}",
            reflection_kind_label(&adaptation.kind),
            truncate_str(&outcome.summary, 120)
        ),
    };
    crate::liquid::reflection::ReflectionEventSummary {
        kind: reflection_kind_label(&outcome.kind),
        turn: outcome.turn.or(adaptation.turn),
        detail,
    }
}

fn is_reflection_adaptation_record(record: &EvolutionRecord) -> bool {
    matches!(record.status.as_str(), "applied" | "enrolled" | "promoted")
        && !is_reflection_adaptation_outcome_record(record)
}

fn is_reflection_adaptation_outcome_record(record: &EvolutionRecord) -> bool {
    matches!(
        record.kind.as_str(),
        "verification" | "failure" | "stall" | "drift"
    )
}

fn reflection_kind_label(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Event".to_string(),
    }
}

fn build_live_auto_reflection_goal_summary(
    state: &AgenticLoopState,
) -> Option<crate::liquid::reflection::GoalSummary> {
    state
        .telemetry
        .observability_session
        .as_ref()
        .and_then(|session| session.read().ok())
        .and_then(|session| {
            let effective_goal = session
                .goal_tracker
                .as_ref()
                .map(|tracker| tracker.goal().to_string())
                .or_else(|| session.original_query.clone())?;
            let progress_summary = session.goal_progress().map(|progress| progress.summary);
            Some(crate::liquid::reflection::GoalSummary {
                effective_goal,
                goal_source: "tracked_goal".into(),
                tracking_status: "tracked_only".into(),
                progress_summary,
            })
        })
}

async fn build_auto_reflection_self_evidence(
    state: &AgenticLoopState,
) -> (
    Option<crate::liquid::reflection::GoalSummary>,
    Option<crate::liquid::reflection::VerificationSummary>,
    Option<crate::liquid::reflection::HealthSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
    Vec<crate::liquid::reflection::ReflectionEventSummary>,
) {
    let mut goal = None;
    let mut verification = None;
    let mut health = None;
    let mut recent_performance_deltas = Vec::new();
    let mut recent_adaptation_impacts = Vec::new();
    let mut recent_adaptation_verification_impacts = Vec::new();
    let mut recent_evaluation_events = Vec::new();
    let mut recent_adaptations = Vec::new();
    let mut recent_adaptation_outcomes = Vec::new();
    if let Some(session_id) = state.current_session_id.as_deref() {
        let service = LocalSelfSurfaceService::new();
        let snapshot = service
            .snapshot(session_id, AUTO_REFLECTION_SELF_EVIDENCE_JOURNAL_LIMIT)
            .await
            .ok();
        let goal_surface = match service
            .surface(
                session_id,
                SelfSurfaceDimension::Goal,
                AUTO_REFLECTION_SELF_EVIDENCE_JOURNAL_LIMIT,
            )
            .await
        {
            Ok(SelfSurfaceResponse::Goal(goal_surface)) => Some(goal_surface),
            _ => None,
        };
        let verification_surface = match service
            .surface(
                session_id,
                SelfSurfaceDimension::Verify,
                AUTO_REFLECTION_SELF_EVIDENCE_JOURNAL_LIMIT,
            )
            .await
        {
            Ok(SelfSurfaceResponse::Verify(verification_surface)) => Some(verification_surface),
            _ => None,
        };
        let health_surface = match service
            .surface(
                session_id,
                SelfSurfaceDimension::Health,
                AUTO_REFLECTION_SELF_EVIDENCE_JOURNAL_LIMIT,
            )
            .await
        {
            Ok(SelfSurfaceResponse::Health(health_surface)) => Some(health_surface),
            _ => None,
        };
        if let Some(snapshot) = snapshot.as_ref() {
            recent_performance_deltas =
                crate::liquid::reflection::summarize_recent_performance_deltas(
                    &snapshot.recent_steps,
                    4,
                );
            recent_adaptation_impacts =
                crate::liquid::reflection::summarize_recent_adaptation_impacts(
                    &snapshot.recent_steps,
                    &snapshot.evolution.records,
                    3,
                );
            if let Some(verification_surface) = verification_surface.as_ref() {
                recent_adaptation_verification_impacts =
                    crate::liquid::reflection::summarize_recent_adaptation_verification_impacts(
                        &verification_surface.objective.recent_verifications,
                        &snapshot.evolution.records,
                        3,
                    );
            }
        }
        recent_evaluation_events = reflection_recent_evaluation_events(
            goal_surface.as_ref(),
            verification_surface.as_ref(),
        );
        recent_adaptations = reflection_recent_adaptations(snapshot.as_ref());
        recent_adaptation_outcomes = reflection_recent_adaptation_outcomes(snapshot.as_ref());
        goal = goal_surface.and_then(reflection_goal_summary_from_surface);
        verification = verification_surface.map(reflection_verification_summary_from_surface);
        health = health_surface.and_then(reflection_health_summary_from_surface);
    }
    if goal.is_none() {
        goal = build_live_auto_reflection_goal_summary(state);
    }
    (
        goal,
        verification,
        health,
        recent_performance_deltas,
        recent_adaptation_impacts,
        recent_adaptation_verification_impacts,
        recent_evaluation_events,
        recent_adaptations,
        recent_adaptation_outcomes,
    )
}

fn apply_auto_reflection_usage(state: &mut AgenticLoopState, result: &HostReflectionResult) {
    state.total_prompt += result.prompt_tokens;
    state.total_completion += result.completion_tokens;
    state.total_cache_read += result.cache_read_tokens;
    state.total_cache_creation += result.cache_creation_tokens;
    if result.has_usage {
        state.has_any_usage = true;
    }
}

pub(crate) async fn maybe_trigger_auto_reflection<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    let evo = match &state.evolution_service {
        Some(e) => Arc::clone(e),
        None => return,
    };

    let (pending_before, applied_before, canary_before, resolved_before) =
        snapshot_evolution_promotion_ids(&evo).await;
    let (fast, llm_signals) = evo.flush().await;
    record_new_evolution_promotion_events(
        state,
        &evo,
        &pending_before,
        &applied_before,
        &canary_before,
        &resolved_before,
    )
    .await;

    if !fast.is_empty() {
        for proposal in fast.iter().take(MAX_RECENT_TACTICAL_ACTIONS) {
            state
                .recent_tactical_actions
                .push(format!("auto-evolution: {}", proposal.reasoning));
        }
        if state.recent_tactical_actions.len() > MAX_RECENT_TACTICAL_ACTIONS {
            let overflow = state.recent_tactical_actions.len() - MAX_RECENT_TACTICAL_ACTIONS;
            state.recent_tactical_actions.drain(..overflow);
        }
    }

    if !llm_signals.is_empty() {
        state.pending_reflection_signals.extend(llm_signals);
    }

    // Inject rule-based pipeline diagnosis (stages::reflect + stages::evaluate)
    // into the tactical-actions context so the LLM reflection prompt sees a
    // structured summary of the runtime's failure mode. Only emit when the
    // diagnosis is non-trivial — avoids noise on healthy loops.
    let diag = crate::turn::agentic_stage_bridge::diagnose_from_loop_state(state);
    if diag.failure_category != astra_pipeline::stages::reflect::FailureCategory::General {
        state
            .recent_tactical_actions
            .push(diag.tactical_action_label());
        if state.recent_tactical_actions.len() > MAX_RECENT_TACTICAL_ACTIONS {
            let overflow = state.recent_tactical_actions.len() - MAX_RECENT_TACTICAL_ACTIONS;
            state.recent_tactical_actions.drain(..overflow);
        }

        // Apply the rule-based strategy delta to the runtime state: block
        // persistently-failing tools so subsequent tool selection excludes
        // them. Report the applied summary via the headless log channel.
        let applied =
            crate::turn::agentic_stage_bridge::apply_strategy_delta(state, &diag.strategy);
        // Publish the applied strategy summary onto the observability session
        // so the SelfModel rendering (edge_tools → edge_profile.self_awareness_text)
        // can surface it back to the agent on the next turn. This closes the
        // passive-self-awareness loop for P2.1 boost/widen signals.
        if !applied.is_noop()
            && let Some(obs) = state.telemetry.observability_session.as_ref()
            && let Ok(mut guard) = obs.write()
        {
            guard.last_strategy_application = Some(applied.clone());
        }
        if !applied.is_noop() {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!("Pipeline strategy applied: {}", applied.summary()),
            );
        }
    }

    // ── Guardrail auto-tuning: observe this turn's tool outcomes and
    //    possibly adjust the reflection threshold before reading it.
    {
        let cursor = state.stall.guardrail_tuner_records_cursor;
        let records = &state.stall.tool_call_records;
        if records.len() > cursor {
            let had_failure = records[cursor..].iter().any(|r| !r.ok);
            state.stall.guardrail_tuner_records_cursor = records.len();
            state.stall.guardrail_tuner.record_turn_outcome(had_failure);
        }
        // Publish current guardrail view onto the observability session so
        // the next SelfModel rendering surfaces it to the agent.
        if let Some(obs) = state.telemetry.observability_session.as_ref()
            && let Ok(mut session) = obs.write()
        {
            let t = &state.stall.guardrail_tuner;
            session.last_guardrail_view = Some(crate::self_model::GuardrailView {
                reflection_threshold: t.reflection_threshold(),
                last_delta: t.last_delta(),
                recent_fail_rate: t.recent_fail_rate(),
                turns_observed: t.turns_seen(),
            });
        }
    }

    if (state.pending_reflection_signals.len() as u32)
        < state.stall.guardrail_tuner.reflection_threshold()
    {
        return;
    }

    if !host.supports_auto_reflection() {
        return;
    }

    let signals = state.pending_reflection_signals.clone();
    let session_id = state.current_session_id.as_deref().unwrap_or("unknown");
    let turns_completed = (state.max_turns - state.remaining_turns) as u32;

    let scenario = state
        .telemetry
        .observability_session
        .as_ref()
        .and_then(|s| s.read().ok())
        .and_then(|s| s.current_scenario())
        .map(|sc| format!("{:?}", sc));

    let token_util = {
        let total = state.total_prompt + state.total_completion;
        let budget = state.max_turn_input_tokens.max(1) as f64;
        let effective_turns = turns_completed.max(1) as f64;
        total as f64 / (budget * effective_turns)
    };
    let tool_stats = build_auto_reflection_tool_stats(state);
    let recent_tactical_actions = state.recent_tactical_actions.clone();
    let active_experiment = build_auto_reflection_experiment_summary(state);

    let (
        goal,
        verification,
        health,
        recent_performance_deltas,
        recent_adaptation_impacts,
        recent_adaptation_verification_impacts,
        recent_evaluation_events,
        recent_adaptations,
        recent_adaptation_outcomes,
    ) = build_auto_reflection_self_evidence(state).await;
    let mut ctx = evo.build_reflection_context(
        session_id,
        turns_completed,
        scenario.as_deref(),
        token_util,
        &signals,
        tool_stats,
        recent_tactical_actions,
        active_experiment,
    );
    ctx.goal = goal;
    ctx.verification = verification;
    ctx.health = health;
    ctx.recent_performance_deltas = recent_performance_deltas;
    ctx.recent_adaptation_impacts = recent_adaptation_impacts;
    ctx.recent_adaptation_verification_impacts = recent_adaptation_verification_impacts;
    ctx.recent_evaluation_events = recent_evaluation_events;
    ctx.recent_adaptations = recent_adaptations;
    ctx.recent_adaptation_outcomes = recent_adaptation_outcomes;

    let (system_prompt, user_prompt) = evo.build_reflection_prompt(&ctx);
    let reflection_result = match host
        .execute_reflection(
            state,
            HostReflectionRequest {
                context: &ctx,
                system_prompt: &system_prompt,
                user_prompt: &user_prompt,
                max_output_tokens: Some(AUTO_REFLECTION_MAX_OUTPUT_TOKENS),
            },
        )
        .await
    {
        Ok(Some(result)) => result,
        Ok(None) => return,
        Err(err) => {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!("Auto-reflection skipped: {err}"),
            );
            return;
        }
    };

    apply_auto_reflection_usage(state, &reflection_result);

    // Record the auto-reflection LLM call so turn.tokens_in breakdown is complete.
    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: reflection_result.prompt_tokens,
            completion_tokens: reflection_result.completion_tokens,
            cache_read_tokens: reflection_result.cache_read_tokens,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("auto_reflection".into()),
            agentic_step: None,
            source: Some("auto_reflection".into()),
            run_id: state.current_run_id.clone(),
        });
    }

    let (pending_before, applied_before, canary_before, resolved_before) =
        snapshot_evolution_promotion_ids(&evo).await;
    match evo
        .ingest_reflection_response_detailed(&reflection_result.full_text, &ctx)
        .await
    {
        Ok(outcome) => {
            record_new_evolution_promotion_events(
                state,
                &evo,
                &pending_before,
                &applied_before,
                &canary_before,
                &resolved_before,
            )
            .await;
            state.pending_reflection_signals.clear();
            state.recent_tactical_actions.clear();
            host.emit_headless_line(
                HeadlessStderrStyle::Green,
                format!(
                    "Auto-reflection processed {} proposal(s): {} auto-applied, {} canary-started, {} queued from {} signal(s).",
                    outcome.processed,
                    outcome.auto_applied,
                    outcome.canary_started,
                    outcome.queued,
                    ctx.signals.len(),
                ),
            );
        }
        Err(err) => {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!("Auto-reflection parse failed: {err}"),
            );
        }
    }
}
