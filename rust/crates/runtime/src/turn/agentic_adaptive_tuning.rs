use std::collections::{HashMap, HashSet};

use astra_services::session_audit::{
    RuntimePromotionController, RuntimePromotionEventData, RuntimePromotionOutcome,
    RuntimePromotionRecommendation,
};

use super::agentic_loop_host::{AgenticLoopOutcome, AgenticLoopState};

pub(crate) const MAX_RECENT_TACTICAL_ACTIONS: usize = 8;
pub(crate) const DEFAULT_TUNING_CYCLE_INTERVAL: u32 = 5;

pub(crate) fn should_emit_adaptive_scenario_event(
    scenario_changed: bool,
    scenario_suppressed: bool,
    config_changes_empty: bool,
) -> bool {
    !scenario_suppressed && (scenario_changed || !config_changes_empty)
}

fn sync_liquid_tactical_runtime(
    state: &mut AgenticLoopState,
    adaptive_enabled: bool,
    turn_token_budget: u64,
) {
    if !adaptive_enabled {
        state.tactical_adapter = None;
        state.step_signal_collector = None;
        return;
    }

    if let Some(ref mut collector) = state.step_signal_collector {
        collector.reset(turn_token_budget);
    } else {
        state.step_signal_collector = Some(crate::liquid::step_signals::StepSignalCollector::new(
            crate::liquid::step_signals::StepSignalConfig::default(),
            turn_token_budget,
        ));
    }

    if state.tactical_adapter.is_none() {
        state.tactical_adapter = Some(crate::liquid::tactical::TacticalAdapter::new(
            crate::liquid::tactical::DampenerConfig::default(),
        ));
    }
}

fn shrink_u32_budget(current: u32, percent: u32, floor: u32) -> u32 {
    let retained = 100_u32.saturating_sub(percent.min(100));
    current
        .saturating_mul(retained)
        .saturating_div(100)
        .max(floor)
}

pub(crate) fn apply_tactical_actions(
    state: &mut AgenticLoopState,
    step_actions: &[crate::liquid::tactical::TacticalAction],
) -> Vec<String> {
    let session = state.telemetry.observability_session.clone();
    let mut session_guard = session.as_ref().and_then(|s| s.write().ok());
    let mut hint_parts = Vec::new();

    for action in step_actions {
        match action {
            crate::liquid::tactical::TacticalAction::IncreaseVerification { reason } => {
                let mut suffix = String::new();
                if let Some(guard) = session_guard.as_mut() {
                    let old = guard.config.verification.strictness;
                    let new = (old + 0.05).min(guard.config.verification.max_strictness);
                    guard.config.verification.strictness = new;
                    if (new - old).abs() > f64::EPSILON {
                        suffix = format!(" (verification {:.2}->{:.2})", old, new);
                    }
                }
                hint_parts.push(format!("⚠️ {reason}{suffix}"));
            }
            crate::liquid::tactical::TacticalAction::SuggestToolSwitch { from_tool, reason } => {
                state.turn_guard.health.force_deprioritize(from_tool);
                hint_parts.push(format!(
                    "💡 Consider switching from '{}': {} (deprioritized for follow-up selection)",
                    from_tool, reason
                ));
            }
            crate::liquid::tactical::TacticalAction::TokenBudgetWarning { used, budget } => {
                let baseline = state
                    .tool_budget_override
                    .or_else(|| {
                        session_guard
                            .as_ref()
                            .map(|guard| guard.config.tool_selection.tool_budget_tokens)
                    })
                    .filter(|&v| v > 0)
                    .unwrap_or(800);
                let new_budget = shrink_u32_budget(baseline, 15, 400);
                state.tool_budget_override = Some(new_budget);

                let mut suffix = format!(" (tool budget {}->{})", baseline, new_budget);
                if let Some(guard) = session_guard.as_mut() {
                    guard.config.tool_selection.tool_budget_tokens = new_budget;

                    let old_threshold = guard.config.compression.compression_threshold;
                    let new_threshold = (old_threshold - 0.05)
                        .max(guard.config.context_window.compression_threshold_min);
                    guard.config.compression.compression_threshold = new_threshold;
                    if (new_threshold - old_threshold).abs() > f64::EPSILON {
                        suffix.push_str(&format!(
                            ", compression {:.2}->{:.2}",
                            old_threshold, new_threshold
                        ));
                    }
                }

                hint_parts.push(format!(
                    "📊 Token usage: {}% of budget consumed. Be concise.{}",
                    used * 100 / budget.max(&1),
                    suffix
                ));
            }
            crate::liquid::tactical::TacticalAction::ThrottleHint { reason } => {
                let mut suffix = String::new();
                if let Some(guard) = session_guard.as_mut() {
                    let old = guard.config.token_budget.max_turn_input_tokens;
                    let new = shrink_u32_budget(old, 10, 30_000);
                    guard.config.token_budget.max_turn_input_tokens = new;
                    state.max_turn_input_tokens = new as u64;
                    if new != old {
                        suffix = format!(" (turn budget {}->{})", old, new);
                    }
                }
                hint_parts.push(format!("🐢 {reason}{suffix}"));
            }
            crate::liquid::tactical::TacticalAction::NoOp => {}
        }
    }

    if !hint_parts.is_empty() {
        state
            .recent_tactical_actions
            .extend(hint_parts.iter().cloned());
        let overflow = state
            .recent_tactical_actions
            .len()
            .saturating_sub(MAX_RECENT_TACTICAL_ACTIONS);
        if overflow > 0 {
            state.recent_tactical_actions.drain(..overflow);
        }
    }

    hint_parts
}

fn carry_forward_tactical_runtime_mutations(
    state: &AgenticLoopState,
    previous_config: &crate::runtime_config::RuntimeConfig,
    next_config: &mut crate::runtime_config::RuntimeConfig,
) {
    next_config.verification.strictness = next_config
        .verification
        .strictness
        .max(previous_config.verification.strictness);
    next_config.compression.compression_threshold = next_config
        .compression
        .compression_threshold
        .min(previous_config.compression.compression_threshold);

    if let Some(tool_budget) = state.tool_budget_override {
        next_config.tool_selection.tool_budget_tokens =
            match next_config.tool_selection.tool_budget_tokens {
                0 => tool_budget,
                current => current.min(tool_budget),
            };
    }

    if state.max_turn_input_tokens > 0 {
        let preserved_turn_budget = state.max_turn_input_tokens.min(u32::MAX as u64) as u32;
        next_config.token_budget.max_turn_input_tokens = next_config
            .token_budget
            .max_turn_input_tokens
            .min(preserved_turn_budget);
    }
}

pub(crate) fn apply_adaptive_execution_profile(state: &mut AgenticLoopState) {
    let (hub, session) = match (
        &state.telemetry.observability_hub,
        &state.telemetry.observability_session,
    ) {
        (Some(hub), Some(session)) => (hub, session),
        _ => return,
    };

    let mut session_guard = match session.write() {
        Ok(guard) => guard,
        Err(_) => return,
    };

    let mut detector = crate::user_profile::ScenarioDetector::new();
    for query in &session_guard.recent_queries {
        detector.observe_query(query);
    }
    for tool in &state.recent_tools {
        detector.observe_tool(tool);
    }

    let routing = crate::pipeline::routing::RoutingEngine::analyze(
        &state.message,
        session_guard.turn_number,
        &state.recent_tools,
        &[],
        Vec::new(),
    );
    let user_id = session_guard.user_id.clone();
    let pattern_library = hub.pattern_library();
    let pattern_library = match pattern_library.as_ref() {
        Some(pattern_library) => match pattern_library.lock() {
            Ok(guard) => Some(guard),
            Err(err) => {
                eprintln!("[adaptive-exec] failed to lock pattern library: {err}");
                None
            }
        },
        None => None,
    };
    let experiments = hub.experiments();

    // Snapshot config before profile application for attribution.
    let old_config = session_guard.config.clone();
    let old_scenario = session_guard.profile.current_scenario;

    let mut profile = crate::adaptive_execution_profile::select_adaptive_execution_profile(
        &session_guard.config,
        &routing,
        &detector,
        Some(hub.adaptive_baselines()),
        pattern_library.as_deref(),
        Some(&*experiments),
        &user_id,
    );

    // ── Anti-flap: scenario change cooldown ──
    // Suppress scenario changes within cooldown period of the last change
    // to prevent rapid oscillation between scenarios.
    let scenario_cooldown = session_guard.config.adaptive_tuning.scenario_cooldown_turns;
    let scenario_suppressed = if profile.scenario != old_scenario && profile.scenario.is_some() {
        if let Some(last_change) = session_guard.last_scenario_change_turn {
            let turns_since = session_guard.turn_number.saturating_sub(last_change);
            if turns_since < scenario_cooldown {
                // Revert to old scenario and config
                profile.scenario = old_scenario;
                profile.config = old_config.clone();
                true
            } else {
                session_guard.last_scenario_change_turn = Some(session_guard.turn_number);
                false
            }
        } else {
            // First scenario change ever — record it
            session_guard.last_scenario_change_turn = Some(session_guard.turn_number);
            false
        }
    } else {
        false
    };

    if let Some(scenario) = profile.scenario {
        session_guard.profile.set_scenario(scenario);
    }
    if let Some(experiment_id) = &profile.experiment_id {
        session_guard
            .profile
            .enroll_experiment(experiment_id.clone());
    }

    carry_forward_tactical_runtime_mutations(state, &old_config, &mut profile.config);

    session_guard.active_experiment_id = profile.experiment_id.clone();
    session_guard.active_variant = profile.variant_id.clone();
    if !scenario_suppressed {
        session_guard.config = profile.config.clone();
    }
    state.max_turn_input_tokens = session_guard.config.token_budget.max_turn_input_tokens as u64;

    // Propagate scenario-driven tool budget override to AgenticLoopState so the
    // CLI host can pass it to build_agentic_tool_selection_context.
    let cfg_budget = session_guard.config.tool_selection.tool_budget_tokens;
    state.tool_budget_override = if cfg_budget > 0 {
        Some(cfg_budget)
    } else {
        None
    };

    // Sync scenario-driven execution limit so the headless round enforces
    // the per-turn tool cap from the active scenario, not the static default.
    state.max_tools_per_turn = session_guard
        .config
        .tool_selection
        .effective_max_tools_per_turn();

    // Collect attribution data while lock is held.
    let turn = session_guard.turn_number;
    let scenario_name = profile
        .scenario
        .map(|s| format!("{s:?}"))
        .unwrap_or_default();
    let confidence = profile.confidence;
    let experiment_id = profile.experiment_id.clone();
    let variant_id = profile.variant_id.clone();
    let scenario_changed = profile.scenario != old_scenario;
    let adaptive_enabled = session_guard.config.context_window.adaptive;
    let turn_token_budget = session_guard.config.token_budget.max_turn_input_tokens as u64;

    // Compute config deltas for journal.
    let mut config_changes = Vec::new();
    if old_config.token_budget.max_turn_input_tokens
        != profile.config.token_budget.max_turn_input_tokens
    {
        config_changes.push((
            "token_budget.max_turn_input_tokens".to_string(),
            old_config.token_budget.max_turn_input_tokens.to_string(),
            profile
                .config
                .token_budget
                .max_turn_input_tokens
                .to_string(),
        ));
    }
    if old_config.memory.retrieval_top_k != profile.config.memory.retrieval_top_k {
        config_changes.push((
            "memory.retrieval_top_k".to_string(),
            old_config.memory.retrieval_top_k.to_string(),
            profile.config.memory.retrieval_top_k.to_string(),
        ));
    }
    if (old_config.verification.strictness - profile.config.verification.strictness).abs() > 0.001 {
        config_changes.push((
            "verification.strictness".to_string(),
            format!("{:.3}", old_config.verification.strictness),
            format!("{:.3}", profile.config.verification.strictness),
        ));
    }
    if old_config.tool_selection.max_tools_per_turn
        != profile.config.tool_selection.max_tools_per_turn
    {
        config_changes.push((
            "tool_selection.max_tools_per_turn".to_string(),
            old_config
                .tool_selection
                .effective_max_tools_per_turn()
                .to_string(),
            profile
                .config
                .tool_selection
                .effective_max_tools_per_turn()
                .to_string(),
        ));
    }
    if old_config.tool_selection.tool_budget_tokens
        != profile.config.tool_selection.tool_budget_tokens
    {
        config_changes.push((
            "tool_selection.tool_budget_tokens".to_string(),
            old_config.tool_selection.tool_budget_tokens.to_string(),
            profile.config.tool_selection.tool_budget_tokens.to_string(),
        ));
    }
    if (old_config.compression.compression_threshold
        - profile.config.compression.compression_threshold)
        .abs()
        > 0.001
    {
        config_changes.push((
            "compression.compression_threshold".to_string(),
            format!("{:.3}", old_config.compression.compression_threshold),
            format!("{:.3}", profile.config.compression.compression_threshold),
        ));
    }
    let baseline_applied = profile.baseline_applied;

    // Release session lock before writing journal.
    drop(session_guard);
    drop(experiments);
    drop(pattern_library);
    sync_liquid_tactical_runtime(state, adaptive_enabled, turn_token_budget);

    // Emit journal event for adaptive profile selection.
    // Skip when scenario is empty (no scenario detected) and no config changes.
    if should_emit_adaptive_scenario_event(
        scenario_changed,
        scenario_suppressed,
        config_changes.is_empty(),
    ) && !scenario_name.is_empty()
    {
        let sid = state.current_session_id.as_deref();
        let event = astra_services::session_journal::JournalEvent::adaptive_scenario_applied(
            sid,
            turn,
            &scenario_name,
            confidence,
            config_changes,
            experiment_id.as_deref(),
            variant_id.as_deref(),
            baseline_applied,
        );
        write_session_journal_event(state, event);
    }

    // Emit separate experiment enrollment event if applicable.
    if let (Some(exp_id), Some(var_id)) = (&experiment_id, &variant_id) {
        let sid = state.current_session_id.as_deref();
        let event = astra_services::session_journal::JournalEvent::adaptive_experiment_enrolled(
            sid, turn, exp_id, var_id, exp_id,
        );
        write_session_journal_event(state, event);
    }
}

/// Per-turn micro-adaptation: reads recent signals and adjusts the session config
/// for the next turn without waiting for a full tuning cycle.
///
/// This is lightweight and runs after each turn completes. It handles:
/// - Token budget adjustment based on burn rate
/// - Compression threshold tightening if compression fired
/// - Memory retrieval expansion on tool churn or drift
/// - Verification strictness increase on corrections
pub(crate) fn apply_per_turn_adaptation(state: &mut AgenticLoopState, turn_tokens_used: u64) {
    let session = match state.telemetry.observability_session.clone() {
        Some(s) => s,
        None => return,
    };

    let mut session_guard = match session.write() {
        Ok(g) => g,
        Err(_) => return,
    };

    // Read immutable session state first to avoid borrow conflicts.
    let compression_count = session_guard.compressed_turns.len();
    let turn = session_guard.turn_number;
    let recent_corrections = session_guard
        .user_corrections
        .iter()
        .filter(|&&t| turn.saturating_sub(t) <= 3)
        .count();

    // Read anti-flap state before mutable config borrow.
    let prev_budget_direction = session_guard.last_token_budget_direction;
    let prev_budget_change_turn = session_guard.last_token_budget_change_turn;

    let config = &mut session_guard.config;

    // Collect changes for attribution journal.
    let mut changes: Vec<(String, String, String)> = Vec::new();
    let mut triggers: Vec<String> = Vec::new();

    // Anti-flap state updates to write back after releasing config borrow.
    let mut new_budget_direction: Option<i8> = None;
    let mut new_budget_change_turn: Option<u32> = None;

    // ── 1. Dynamic token budget ──
    // Anti-flap: detect direction oscillation and suppress rapid reversals.
    let budget_cooldown = config.adaptive_tuning.budget_cooldown_turns;
    if config.context_window.adaptive && turn_tokens_used > 0 {
        let max_budget = config.token_budget.max_turn_input_tokens;
        let threshold = (max_budget as f64 * 0.85) as u64;
        if turn_tokens_used > threshold && max_budget > 30_000 {
            // Check for oscillation: if the previous change was an increase and
            // it happened recently, skip this decrease to prevent ping-pong.
            let oscillation_suppressed = if prev_budget_direction > 0 {
                if let Some(last_turn) = prev_budget_change_turn {
                    turn.saturating_sub(last_turn) < budget_cooldown
                } else {
                    false
                }
            } else {
                false
            };

            if !oscillation_suppressed {
                let old = max_budget;
                let reduction = ((turn_tokens_used as f64 * 0.1) as u32).min(10_000);
                config.token_budget.max_turn_input_tokens = config
                    .token_budget
                    .max_turn_input_tokens
                    .saturating_sub(reduction)
                    .max(30_000);
                let new = config.token_budget.max_turn_input_tokens;
                if new != old {
                    new_budget_direction = Some(-1);
                    new_budget_change_turn = Some(turn);
                    changes.push((
                        "token_budget.max_turn_input_tokens".into(),
                        old.to_string(),
                        new.to_string(),
                    ));
                    triggers.push(format!(
                        "token burn {:.0}% ({}k/{}k)",
                        turn_tokens_used as f64 / old as f64 * 100.0,
                        turn_tokens_used / 1000,
                        old / 1000,
                    ));
                }
            }
        }
    }

    // ── 2. Dynamic compression threshold ──
    if config.context_window.dynamic_compression && compression_count > 1 {
        let old = config.compression.compression_threshold;
        let new_threshold = (config.compression.compression_threshold - 0.05)
            .max(config.context_window.compression_threshold_min);
        config.compression.compression_threshold = new_threshold;
        if (new_threshold - old).abs() > 0.001 {
            changes.push((
                "compression.compression_threshold".into(),
                format!("{old:.3}"),
                format!("{new_threshold:.3}"),
            ));
            triggers.push(format!("{compression_count} compressions"));
        }
    }

    // ── 3. Memory pressure expansion on corrections ──
    if config.memory_pressure.adaptive
        && config.memory_pressure.expand_on_correction
        && recent_corrections > 0
    {
        let old = config.memory.retrieval_top_k;
        let new_k = (config.memory.retrieval_top_k + 1).min(config.memory_pressure.retrieval_max);
        config.memory.retrieval_top_k = new_k;
        if new_k != old {
            changes.push((
                "memory.retrieval_top_k".into(),
                old.to_string(),
                new_k.to_string(),
            ));
            triggers.push(format!("{recent_corrections} recent correction(s)"));
        }
    }

    // ── 4. Verification strictness on corrections ──
    if config.verification.adaptive
        && config.verification.increase_on_correction
        && recent_corrections >= 1
    {
        let old = config.verification.strictness;
        let new_strictness =
            (config.verification.strictness + 0.05).min(config.verification.max_strictness);
        config.verification.strictness = new_strictness;
        if (new_strictness - old).abs() > 0.001 {
            changes.push((
                "verification.strictness".into(),
                format!("{old:.3}"),
                format!("{new_strictness:.3}"),
            ));
            triggers.push(format!("{recent_corrections} recent correction(s)"));
        }
    }

    // Sync the loop-level token budget with the updated config
    state.max_turn_input_tokens = config.token_budget.max_turn_input_tokens as u64;

    // Write back anti-flap state (deferred to avoid borrow conflict with config).
    if let Some(dir) = new_budget_direction {
        session_guard.last_token_budget_direction = dir;
    }
    if let Some(t) = new_budget_change_turn {
        session_guard.last_token_budget_change_turn = Some(t);
    }

    // Release lock before writing journal.
    drop(session_guard);

    // Emit journal event if anything changed.
    if !changes.is_empty() {
        // De-duplicate triggers
        triggers.sort();
        triggers.dedup();
        let sid = state.current_session_id.as_deref();
        let event = astra_services::session_journal::JournalEvent::adaptive_per_turn_applied(
            sid, turn, changes, triggers,
        );
        write_session_journal_event(state, event);
    }
}

fn runtime_promotion_recommendation(
    recommendation: crate::evolution::types::ProposalPromotionRecommendation,
) -> RuntimePromotionRecommendation {
    match recommendation {
        crate::evolution::types::ProposalPromotionRecommendation::Promote => {
            RuntimePromotionRecommendation::Promote
        }
        crate::evolution::types::ProposalPromotionRecommendation::Canary => {
            RuntimePromotionRecommendation::Canary
        }
        crate::evolution::types::ProposalPromotionRecommendation::Hold => {
            RuntimePromotionRecommendation::Hold
        }
    }
}

fn record_runtime_promotion_event(state: &mut AgenticLoopState, event: RuntimePromotionEventData) {
    let already_recorded = state.telemetry.promotion_events.iter().any(|existing| {
        existing.controller == event.controller
            && existing.outcome == event.outcome
            && existing.subject_id == event.subject_id
    });
    if !already_recorded {
        state.telemetry.promotion_events.push(event);
    }
}

fn record_adaptive_baseline_event(
    state: &mut AgenticLoopState,
    outcome: RuntimePromotionOutcome,
    experiment_id: &str,
    variant_id: &str,
    verdict: &crate::adaptive_baselines::AdaptiveBaselinePromotionVerdict,
) {
    record_runtime_promotion_event(
        state,
        RuntimePromotionEventData {
            controller: RuntimePromotionController::AdaptiveBaseline,
            outcome,
            recommendation: runtime_promotion_recommendation(verdict.recommendation),
            subject_id: experiment_id.to_string(),
            summary: format!(
                "adaptive baseline winner '{variant_id}' for experiment '{experiment_id}'"
            ),
            turn: None,
            confidence_score: verdict.confidence_score,
            support_score: verdict.support_score,
            safety_score: verdict.safety_score,
            overall_score: verdict.overall_score,
            blockers: verdict.blockers.clone(),
            evidence: verdict.evidence.clone(),
            rollback_hint: verdict.rollback_hint.clone(),
            run_id: state.current_run_id.clone(),
        },
    );
}

fn record_evolution_proposal_event(
    state: &mut AgenticLoopState,
    outcome: RuntimePromotionOutcome,
    proposal: &crate::evolution::types::EvolutionProposal,
) {
    let Some(verdict) = proposal.promotion_verdict.as_ref() else {
        return;
    };
    record_runtime_promotion_event(
        state,
        RuntimePromotionEventData {
            controller: RuntimePromotionController::Evolution,
            outcome,
            recommendation: runtime_promotion_recommendation(verdict.recommendation),
            subject_id: proposal.id.clone(),
            summary: proposal.reasoning.clone(),
            turn: None,
            confidence_score: verdict.confidence_score,
            support_score: verdict.support_score,
            safety_score: verdict.safety_score,
            overall_score: verdict.overall_score,
            blockers: verdict.blockers.clone(),
            evidence: verdict.evidence.clone(),
            rollback_hint: verdict.rollback_hint.clone(),
            run_id: state.current_run_id.clone(),
        },
    );
}

pub(crate) async fn snapshot_evolution_promotion_ids(
    evo: &crate::evolution::service::EvolutionService,
) -> (
    HashSet<String>,
    HashSet<String>,
    HashMap<String, crate::evolution::types::EvolutionProposal>,
    HashSet<String>,
) {
    let pending = evo
        .pending()
        .await
        .into_iter()
        .map(|proposal| proposal.id)
        .collect::<HashSet<_>>();
    let applied = evo
        .applied()
        .await
        .into_iter()
        .map(|proposal| proposal.id)
        .collect::<HashSet<_>>();
    let canary = evo
        .active_canaries()
        .await
        .into_iter()
        .map(|proposal| (proposal.id.clone(), proposal))
        .collect::<HashMap<_, _>>();
    let resolved = evo
        .resolved_canaries()
        .await
        .into_iter()
        .map(|proposal| proposal.id)
        .collect::<HashSet<_>>();
    (pending, applied, canary, resolved)
}

pub(crate) async fn record_new_evolution_promotion_events(
    state: &mut AgenticLoopState,
    evo: &crate::evolution::service::EvolutionService,
    pending_before: &HashSet<String>,
    applied_before: &HashSet<String>,
    canary_before: &HashMap<String, crate::evolution::types::EvolutionProposal>,
    resolved_before: &HashSet<String>,
) {
    for proposal in evo.pending().await {
        if !pending_before.contains(&proposal.id) {
            record_evolution_proposal_event(state, RuntimePromotionOutcome::Queued, &proposal);
        }
    }
    let applied_after = evo.applied().await;
    for proposal in applied_after {
        if applied_before.contains(&proposal.id) || canary_before.contains_key(&proposal.id) {
            continue;
        }
        record_evolution_proposal_event(state, RuntimePromotionOutcome::AutoApplied, &proposal);
    }
    let active_after = evo.active_canaries().await;
    for proposal in active_after {
        if !canary_before.contains_key(&proposal.id) {
            record_evolution_proposal_event(
                state,
                RuntimePromotionOutcome::CanaryStarted,
                &proposal,
            );
        }
    }
    for proposal in evo.resolved_canaries().await {
        if resolved_before.contains(&proposal.id) {
            continue;
        }
        let outcome = match proposal.status {
            crate::evolution::types::ApprovalStatus::CanaryPromoted => {
                RuntimePromotionOutcome::CanaryPromoted
            }
            crate::evolution::types::ApprovalStatus::CanaryRolledBack => {
                RuntimePromotionOutcome::CanaryRolledBack
            }
            _ => continue,
        };
        record_evolution_proposal_event(state, outcome, &proposal);
    }
}

fn effective_tool_metrics(state: &AgenticLoopState) -> (u32, u32) {
    if state.stall.tool_call_records.is_empty() {
        return (
            state.total_tool_calls,
            state.telemetry.all_tools_used.len() as u32,
        );
    }

    let mut unique_tools = HashSet::new();
    let mut tool_calls = 0u32;
    for record in &state.stall.tool_call_records {
        if record.is_synthetic_placeholder() {
            continue;
        }
        tool_calls += 1;
        unique_tools.insert(record.name.clone());
    }

    (tool_calls, unique_tools.len() as u32)
}

/// Record feedback signals based on the loop's outcome and accumulated state.
///
/// Called once after the loop finishes (or errors) to feed the auto-tuning engine.
pub(crate) fn record_loop_completion_feedback(
    state: &mut AgenticLoopState,
    result: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
) {
    use crate::auto_tuning::{FeedbackSignal, SignalType};

    let hub = match &state.telemetry.observability_hub {
        Some(h) => h,
        None => return,
    };

    let turn_id = state.current_run_id.clone().unwrap_or_default();
    let session_attribution = state
        .telemetry
        .observability_session
        .as_ref()
        .map(|session| {
            let session = session.read().unwrap_or_else(|e| e.into_inner());
            crate::observability_integration::session_signal_attribution(&session)
        });
    let enrich_signal = |signal: FeedbackSignal| {
        let mut signal = crate::observability_integration::with_signal_attribution(
            signal,
            session_attribution.as_ref(),
        );
        if !signal.context.contains_key("session_id")
            && let Some(session_id) = state.current_session_id.as_deref()
        {
            signal = signal.with_context("session_id", serde_json::json!(session_id));
        }
        signal
    };

    // ── 1. Outcome signal ──
    match result {
        Ok(AgenticLoopOutcome::Completed) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskSuccess).with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Cancelled) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::Interruption).with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Error(reason)) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: reason.clone(),
                })
                .with_turn(&turn_id),
            ));
        }
        Err(reason) => {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: reason.to_string(),
                })
                .with_turn(&turn_id),
            ));
        }
        Ok(AgenticLoopOutcome::Waiting(_)) => {
            // No signal for waiting — the loop will resume.
        }
    }

    // ── 2. Token usage signal ──
    let total_tokens = state.total_prompt + state.total_completion;
    // Heuristic threshold: >50k tokens suggests inefficiency for most tasks.
    let token_threshold = 50_000u64;
    if total_tokens > token_threshold {
        hub.record_feedback(enrich_signal(
            FeedbackSignal::new(SignalType::HighTokenUsage {
                tokens: total_tokens,
                threshold: token_threshold,
            })
            .with_turn(&turn_id),
        ));
    }

    // ── 3. Tool churn signal ──
    let (tool_calls, unique_tools) = effective_tool_metrics(state);
    // High tool calls with low unique tools suggests repetitive/failing usage.
    if tool_calls > 10 && unique_tools > 0 && (tool_calls / unique_tools) > 5 {
        hub.record_feedback(enrich_signal(
            FeedbackSignal::new(SignalType::ToolChurn {
                calls: tool_calls,
                unique_tools,
            })
            .with_turn(&turn_id),
        ));
    }

    // ── 4. Tool-level failure signals ──
    let failed_tools: u32 = state
        .stall
        .tool_call_records
        .iter()
        .filter(|r| !r.ok)
        .count() as u32;
    if failed_tools > 0 && tool_calls > 0 {
        let failure_rate = failed_tools as f64 / tool_calls as f64;
        if failure_rate > 0.3 {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: format!(
                        "high tool failure rate: {failed_tools}/{tool_calls} ({:.0}%)",
                        failure_rate * 100.0
                    ),
                })
                .with_turn(&turn_id)
                .with_context("tool_failure_rate", serde_json::json!(failure_rate)),
            ));
        }
    }

    // ── 5. Skill quality signals ──
    for (name, entry) in state.skills.quality_tracker.all_entries() {
        if entry.invocations == 0 {
            continue;
        }
        if entry.failures > 0 {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskFailure {
                    reason: format!(
                        "skill '{}' failed {}/{} invocations",
                        name, entry.failures, entry.invocations
                    ),
                })
                .with_turn(&turn_id)
                .with_context("skill_name", serde_json::json!(name))
                .with_context(
                    "skill_success_rate",
                    serde_json::json!(entry.success_rate()),
                ),
            ));
        } else {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::TaskSuccess)
                    .with_turn(&turn_id)
                    .with_context("skill_name", serde_json::json!(name))
                    .with_context("skill_invocations", serde_json::json!(entry.invocations)),
            ));
        }
    }

    // ── 6. Retry detection signal ──
    // Detect consecutive identical tool calls (same name + similar args) as retry behavior.
    {
        let records = &state.stall.tool_call_records;
        if records.len() >= 2 {
            let mut consecutive = 1u32;
            for pair in records.windows(2).rev() {
                if pair[0].name == pair[1].name
                    && pair[0].args_preview == pair[1].args_preview
                    && !pair[1].ok
                {
                    consecutive += 1;
                } else {
                    break;
                }
            }
            if consecutive >= 2 {
                hub.record_feedback(enrich_signal(
                    FeedbackSignal::new(SignalType::Retry { count: consecutive })
                        .with_turn(&turn_id)
                        .with_context(
                            "tool_name",
                            serde_json::json!(records.last().map(|r| &r.name)),
                        ),
                ));
            }
        }
    }

    // ── 7. Acceptance signal ──
    // If there is a prior assistant message and the current user message shows no correction
    // intent, emit Acceptance — the user implicitly accepted the previous output.
    {
        let has_prior_assistant = state
            .messages
            .iter()
            .rev()
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
        if has_prior_assistant && !state.message.is_empty() {
            let lower = state.message.to_lowercase();
            let is_correction = crate::evolution::signal_collector::CORRECTION_KEYWORDS
                .iter()
                .any(|kw| lower.contains(kw));
            if !is_correction {
                hub.record_feedback(enrich_signal(
                    FeedbackSignal::new(SignalType::Acceptance).with_turn(&turn_id),
                ));
            }
        }
    }

    // ── 8. Tool health signals ──
    // Emit signals for deprioritized tools so tuning rules can react.
    {
        let deprioritized = state.turn_guard.health.deprioritized_tools();
        for tool_name in deprioritized {
            hub.record_feedback(enrich_signal(
                FeedbackSignal::new(SignalType::ToolDeprioritized {
                    tool_name: tool_name.to_string(),
                })
                .with_turn(&turn_id),
            ));
        }
        // Track tools that were rehabilitated this session (rehab count > 0 but not deprioritized).
        for (name, health) in state.turn_guard.health.all() {
            if health.rehabilitation_count > 0 && !health.deprioritized {
                hub.record_feedback(enrich_signal(
                    FeedbackSignal::new(SignalType::ToolRehabilitated {
                        tool_name: name.clone(),
                    })
                    .with_turn(&turn_id),
                ));
            }
        }
    }
}

///
/// Every N turns (configured via `adaptive_tuning.tuning_cycle_interval`),
/// evaluates all registered evolution rules and applies any triggered actions
/// to the session's RuntimeConfig.
pub(crate) fn maybe_run_tuning_cycle(state: &mut AgenticLoopState) {
    // Read the interval from session config if available, else use default.
    let interval = state
        .telemetry
        .observability_session
        .as_ref()
        .and_then(|s| s.read().ok())
        .map(|g| g.config.adaptive_tuning.tuning_cycle_interval)
        .unwrap_or(DEFAULT_TUNING_CYCLE_INTERVAL);

    if state.telemetry.completed_turns_for_tuning < interval {
        return;
    }
    state.telemetry.completed_turns_for_tuning = 0;

    let hub = match state.telemetry.observability_hub.clone() {
        Some(h) => h,
        None => return,
    };

    let session = match state.telemetry.observability_session.clone() {
        Some(s) => s,
        None => return,
    };

    let mut session_guard = match session.write() {
        Ok(g) => g,
        Err(_) => return,
    };

    let actions = hub.run_tuning_cycle(&mut session_guard.config);
    let turn = session_guard.turn_number;

    // Track token-budget direction changes from tuning rules for anti-flap.
    let new_budget = session_guard.config.token_budget.max_turn_input_tokens;
    let old_budget_before_tuning = {
        // Compare against what it was before this cycle
        state.max_turn_input_tokens as u32
    };
    if new_budget != old_budget_before_tuning {
        let direction: i8 = if new_budget > old_budget_before_tuning {
            1
        } else {
            -1
        };
        session_guard.last_token_budget_direction = direction;
        session_guard.last_token_budget_change_turn = Some(turn);
    }

    if !actions.is_empty() {
        eprintln!(
            "[auto-tuning] cycle applied {} rule(s): {:?}",
            actions.len(),
            actions
        );
    }

    // Release lock before writing journal events.
    let action_ids = actions.clone();
    drop(session_guard);

    // Emit journal events for triggered rules.
    for rule_id in &action_ids {
        let event = astra_services::session_journal::JournalEvent::adaptive_tuning_rule_triggered(
            state.current_session_id.as_deref(),
            turn,
            rule_id,
            rule_id,
            "aggregate",
            Vec::new(),
        );
        write_session_journal_event(state, event);
    }

    // Re-acquire lock for remaining operations.
    let session = match &state.telemetry.observability_session {
        Some(s) => s,
        None => return,
    };
    let mut session_guard = match session.write() {
        Ok(g) => g,
        Err(_) => return,
    };

    // Check if any previously applied rules should be rolled back
    let rollbacks = hub.check_rollbacks(&mut session_guard.config);
    if !rollbacks.is_empty() {
        eprintln!(
            "[auto-tuning] rolled back {} rule(s): {:?}",
            rollbacks.len(),
            rollbacks
        );
    }

    // Persist feedback state after each tuning cycle
    if let Err(e) = crate::auto_tuning::save_feedback("default", hub.tuning()) {
        eprintln!("[auto-tuning] failed to persist feedback: {e}");
    }
    drop(session_guard);

    let exploration = crate::exploration_engine::ExplorationEngine::default();
    let created = match hub.pattern_library() {
        Some(pattern_library) => match pattern_library.lock() {
            Ok(pattern_library) => {
                let experiments = hub.experiments();
                exploration.check_and_create_experiments(&pattern_library, &experiments)
            }
            Err(err) => {
                eprintln!("[adaptive-exec] failed to lock pattern library: {err}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    if !created.is_empty() {
        eprintln!(
            "[adaptive-exec] created {} experiment(s): {:?}",
            created.len(),
            created.iter().map(|exp| &exp.id).collect::<Vec<_>>()
        );
    }

    let concluded = {
        let experiments = hub.experiments();
        exploration.conclude_mature_experiments(&experiments)
    };
    if !concluded.is_empty() {
        eprintln!(
            "[adaptive-exec] concluded {} experiment(s): {:?}",
            concluded.len(),
            concluded
                .iter()
                .map(|c| (&c.experiment_id, &c.winner_variant_id))
                .collect::<Vec<_>>()
        );
    }
    let mut promoted = Vec::new();
    let mut deferred = Vec::new();
    for conclusion in &concluded {
        let Some(winner_variant_id) = conclusion.winner_variant_id.as_deref() else {
            continue;
        };
        match hub.promote_experiment_winner_with_signals(
            &conclusion.experiment_id,
            winner_variant_id,
            state.telemetry.runtime_promotion_signals.as_ref(),
        ) {
            Ok(crate::adaptive_baselines::AdaptiveBaselinePromotionDecision::Promoted {
                promotion,
                verdict,
            }) => {
                record_adaptive_baseline_event(
                    state,
                    RuntimePromotionOutcome::Promoted,
                    &conclusion.experiment_id,
                    winner_variant_id,
                    &verdict,
                );
                promoted.push(promotion);
            }
            Ok(crate::adaptive_baselines::AdaptiveBaselinePromotionDecision::Deferred(verdict)) => {
                record_adaptive_baseline_event(
                    state,
                    RuntimePromotionOutcome::Deferred,
                    &conclusion.experiment_id,
                    winner_variant_id,
                    &verdict,
                );
                deferred.push((
                    conclusion.experiment_id.clone(),
                    winner_variant_id.to_string(),
                    verdict.recommendation,
                    verdict.blockers,
                ));
            }
            Ok(crate::adaptive_baselines::AdaptiveBaselinePromotionDecision::Skipped) => {}
            Err(err) => eprintln!(
                "[adaptive-exec] failed to promote winner for {}: {err}",
                conclusion.experiment_id
            ),
        }
    }
    if !promoted.is_empty() {
        eprintln!(
            "[adaptive-exec] promoted {} adaptive baseline(s): {:?}",
            promoted.len(),
            promoted
                .iter()
                .map(|p| (&p.scope.task_type, &p.scope.domain, &p.variant_id))
                .collect::<Vec<_>>()
        );
        for promotion in &promoted {
            write_session_journal_event(
                state,
                astra_services::session_journal::JournalEvent::adaptive_baseline_promoted(
                    state.current_session_id.as_deref(),
                    &promotion.scope.task_type,
                    promotion.scope.domain.as_deref(),
                    &promotion.experiment_id,
                    &promotion.variant_id,
                    promotion.replaced_existing,
                    &promotion.config_keys,
                ),
            );
        }
    }
    if !deferred.is_empty() {
        eprintln!(
            "[adaptive-exec] deferred {} adaptive baseline promotion(s): {:?}",
            deferred.len(),
            deferred
                .iter()
                .map(|(experiment_id, variant_id, recommendation, blockers)| (
                    experiment_id,
                    variant_id,
                    recommendation,
                    blockers
                ))
                .collect::<Vec<_>>()
        );
    }
}

fn write_session_journal_event(
    state: &AgenticLoopState,
    event: astra_services::session_journal::JournalEvent,
) {
    let Some(session_id) = state.current_session_id.as_deref() else {
        return;
    };
    let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) else {
        return;
    };
    let _ = writer.append(&event);
}
