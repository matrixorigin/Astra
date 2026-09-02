use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::super::{CompactionEngine, TokenBudget};
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, BudgetWrapupOrigin,
    CompletionActionWindow, RunControlStatus, SkillAutoRouteJudgeContext, TurnIntentJudgeOutcome,
    TurnPhaseKind, TurnPhaseOutcome, complete_turn_phase, try_write_heavy_checkpoint,
};
use crate::orchestration::permission_sync::PermissionResponseMessaging;
use crate::orchestration::{
    AgentToolRecordActionKind, CANCELLATION_ORIGIN_UNVERIFIED, CancellationOrigin,
    project_agent_tool_budget_record, render_agent_tool_budget_unfinished_detail,
    summarize_agent_tool_budget_result,
};
use astra_config::user_profile::{
    Scenario, TurnIntent, WorkLifecycleIntent, WorkspaceMutationIntent,
};
use astra_services::SessionArtifactStore;
use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind, CompactionTier};
use astra_turn_core::interruption::{
    InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
};
use astra_turn_core::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;

const CHILD_AGENT_CANCEL_TIMEOUT: Duration = Duration::from_secs(3);
const PAUSE_LOOP_LOCAL_CHECK_INTERVAL: Duration = Duration::from_millis(25);
const PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAILBOX_MODEL_PREVIEW_CHARS: usize = 4_000;

fn push_mailbox_model_preview(parts: &mut Vec<String>, value: String) {
    let mut chars = value.chars();
    let preview = chars
        .by_ref()
        .take(MAILBOX_MODEL_PREVIEW_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        parts.push(format!(
            "{preview}… [message preview truncated; use agent.get_result for a full child terminal result]"
        ));
    } else {
        parts.push(preview);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TurnIterationPrep {
    pub(crate) quiet: bool,
    pub(crate) turn_start_time: Instant,
}

pub(crate) enum PreparedTurnIteration {
    Ready(TurnIterationPrep),
    Finished(AgenticLoopOutcome),
}

/// Wait until a live pause clears or cancellation wins. This is the sole
/// pause-control wait primitive for both turn preparation and provider
/// boundaries: local flags give prompt same-pod wakeups while the bounded
/// durable poll preserves correct cross-pod resume/cancel behavior.
pub(crate) async fn wait_for_pause_clear_or_cancel(state: &mut AgenticLoopState) -> bool {
    let mut last_db_poll = tokio::time::Instant::now();
    while state
        .cancellation
        .pause_flag
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        if state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        {
            return true;
        }

        if last_db_poll.elapsed() >= PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL {
            if let Some(run_control) = state.run_control.as_ref() {
                if let (Some(user_id), Some(run_id)) = (
                    state.context_manifest_user_id.as_deref(),
                    state.current_run_id.as_deref(),
                ) {
                    match run_control.control_status(user_id, run_id).await {
                        Ok(Some(RunControlStatus::Cancelled)) => {
                            if let Some(flag) = state.cancellation.flag.as_ref() {
                                flag.store(true, Ordering::SeqCst);
                            }
                            if let Some(token) = state.cancellation.token.as_ref() {
                                token.cancel();
                            }
                            return true;
                        }
                        Ok(Some(RunControlStatus::Paused)) => {}
                        Ok(None) => {
                            if let Some(flag) = state.cancellation.pause_flag.as_ref() {
                                flag.store(false, Ordering::SeqCst);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                run_id,
                                error = %error,
                                "failed to poll run control status while paused"
                            );
                        }
                    }
                }
            }
            last_db_poll = tokio::time::Instant::now();
        }
        tokio::time::sleep(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
    }
    false
}

fn should_complete_budget_exhaustion_gracefully(state: &AgenticLoopState) -> bool {
    state.total_tool_calls > 0
        || state.total_prompt > 0
        || state.total_completion > 0
        || state.stall.last_heavy_checkpoint.is_some()
}

pub(crate) fn session_turn_number(state: &AgenticLoopState) -> u32 {
    state.current_session_turn_number()
}

pub(crate) fn current_agentic_step(state: &AgenticLoopState) -> u32 {
    if state.llm_rounds_completed > 0 {
        return state.llm_rounds_completed;
    }
    state.max_turns.saturating_sub(state.remaining_turns) as u32
}

pub(crate) fn completed_tool_calls(state: &AgenticLoopState) -> u32 {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed())
        .count()
        .min(u32::MAX as usize) as u32
}

fn apply_judged_turn_intent_to_observability(
    state: &AgenticLoopState,
    intent: &TurnIntent,
    record_feedback: bool,
) {
    if let Some(session) = &state.telemetry.observability_session {
        let mut session = astra_core::sync_poison::recover_rwlock_write(session);
        session.profile.apply_judged_turn_intent(intent);
        let is_correction = intent.objective_relation
            == astra_turn_types::ObjectiveRelation::Correct
            || intent.feedback.is_some_and(|feedback| {
                feedback.kind == astra_turn_types::UserFeedbackKind::Correction
            });
        if record_feedback && is_correction && session.record_user_correction() {
            let correction = if state.user_intent.trim().is_empty() {
                state.message.as_str()
            } else {
                state.user_intent.as_str()
            };
            session.record_correction_excerpt(correction);
        }
    }

    let Some(hub) = &state.telemetry.observability_hub else {
        return;
    };
    if !record_feedback {
        return;
    }
    let signal_type = match intent.feedback.map(|feedback| feedback.kind) {
        Some(astra_turn_types::UserFeedbackKind::Approval) => {
            astra_core::feedback::SignalType::Acceptance
        }
        Some(astra_turn_types::UserFeedbackKind::Correction) => {
            astra_core::feedback::SignalType::Correction
        }
        Some(
            astra_turn_types::UserFeedbackKind::Clarification
            | astra_turn_types::UserFeedbackKind::Requirement
            | astra_turn_types::UserFeedbackKind::Preference,
        ) => astra_core::feedback::SignalType::Reanchor,
        None => match intent.objective_relation {
            astra_turn_types::ObjectiveRelation::Acknowledge => {
                astra_core::feedback::SignalType::Acceptance
            }
            astra_turn_types::ObjectiveRelation::Correct => {
                astra_core::feedback::SignalType::Correction
            }
            astra_turn_types::ObjectiveRelation::Unknown
            | astra_turn_types::ObjectiveRelation::Continue
            | astra_turn_types::ObjectiveRelation::Refine
            | astra_turn_types::ObjectiveRelation::Replace => return,
        },
    };

    let mut signal = astra_core::feedback::FeedbackSignal::new(signal_type).with_context(
        "objective_relation",
        serde_json::json!(intent.objective_relation.as_str()),
    );
    if let Some(feedback) = intent.feedback {
        signal = signal
            .with_context("feedback_kind", serde_json::json!(feedback.kind.as_str()))
            .with_context(
                "feedback_target",
                serde_json::json!(feedback.target.as_str()),
            );
    }
    if let Some(run_id) = state.current_run_id.as_deref() {
        signal = signal.with_turn(run_id);
    }
    if let Some(session_id) = state.current_session_id.as_deref() {
        signal = signal.with_context("session_id", serde_json::json!(session_id));
    }
    hub.record_feedback(signal);
}

fn record_current_user_turn_semantics(state: &mut AgenticLoopState, intent: &TurnIntent) -> bool {
    // A single submitted user turn owns one marker. The agentic loop may call
    // this preparation phase more than once, and additional user guidance may
    // arrive between rounds. Resolve the canonical owner from the submitted
    // input rather than the mutable agent-loop counter or the latest user
    // message, either of which can advance inside the same user turn.
    let submitted_inputs = [state.user_intent.trim(), state.message.trim()];
    let matching_owner = state
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if message.get("role").and_then(Value::as_str) != Some("user")
                || astra_turn_types::is_runtime_owned_message(message)
            {
                return None;
            }
            let content = astra_turn_core::prompt_facing::extract_text_content(message)?;
            submitted_inputs
                .iter()
                .any(|submitted| !submitted.is_empty() && content.trim() == *submitted)
                .then_some(index)
        });
    let Some(index) = matching_owner else {
        tracing::warn!("turn intent was judged without an exact canonical user-message owner");
        return false;
    };

    let semantics =
        astra_turn_types::UserTurnSemantics::new(intent.objective_relation, intent.feedback);

    match astra_turn_types::user_turn_semantics(&state.messages[index]) {
        Ok(Some(current)) => {
            let advances_unknown = current.objective_relation
                == astra_turn_types::ObjectiveRelation::Unknown
                && current.feedback.is_none()
                && (intent.objective_relation != astra_turn_types::ObjectiveRelation::Unknown
                    || intent.feedback.is_some());
            if !advances_unknown {
                return false;
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                error = %error,
                "refusing to overwrite invalid producer-owned user-turn semantics"
            );
            return false;
        }
    }
    let message = &mut state.messages[index];
    let recorded = astra_turn_types::mark_user_turn_semantics(message, semantics);
    if !recorded {
        tracing::warn!("canonical turn-semantics owner was not a user message");
    }
    recorded
}

fn allowed_requested_scenario(intent: &TurnIntent) -> Option<Scenario> {
    intent
        .requested_scenario
        .filter(|scenario| intent.allows_scenario(*scenario))
}

fn task_profile_from_judged_turn_intent(
    intent: &TurnIntent,
) -> Option<astra_turn_core::chat_turn_heuristics::TaskExecutionProfile> {
    let scenario = allowed_requested_scenario(intent);
    let has_control_signal = scenario.is_some()
        || intent.workspace_mutation != WorkspaceMutationIntent::Unknown
        || intent.browser_verification_required;
    if !has_control_signal {
        return None;
    }

    let mutates_workspace = intent.requires_workspace_mutation();
    let exploratory_task = matches!(
        scenario,
        Some(
            Scenario::CodeReview | Scenario::Debugging | Scenario::Exploration | Scenario::Testing
        )
    );
    let complexity = if matches!(scenario, Some(Scenario::Refactoring | Scenario::DevOps)) {
        astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex
    } else {
        astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard
    };
    Some(
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            mutates_workspace,
            exploratory_task,
            complexity,
        ),
    )
}

fn apply_judged_turn_intent_to_runtime_profile(state: &mut AgenticLoopState, intent: &TurnIntent) {
    state.turn_intent = Some(intent.clone());
    let Some(profile) = task_profile_from_judged_turn_intent(intent) else {
        return;
    };
    let previous_profile = state.task_profile;
    let previous_budget = state.agentic_turn_budget;
    let budget_followed_previous_profile = previous_budget == previous_profile.agentic_turn_budget;
    state.task_profile = profile;
    if budget_followed_previous_profile {
        state.agentic_turn_budget = profile.agentic_turn_budget;
        if state.max_turns == previous_budget.initial_turns {
            let new_initial = profile.agentic_turn_budget.initial_turns;
            if new_initial >= previous_budget.initial_turns {
                let extra = new_initial - previous_budget.initial_turns;
                state.max_turns = new_initial;
                state.remaining_turns = state.remaining_turns.saturating_add(extra);
            } else {
                let reduction = previous_budget.initial_turns - new_initial;
                state.max_turns = new_initial;
                state.remaining_turns = state.remaining_turns.saturating_sub(reduction);
            }
        }
    }
    state.turn_guard.set_task_profile(profile);
}

fn auto_route_tool_call_id(skill_name: &str) -> String {
    let mut normalized = String::with_capacity(skill_name.len());
    let mut last_was_dash = true;
    for ch in skill_name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "skill-auto-route".to_string()
    } else {
        format!("skill-auto-route-{normalized}")
    }
}

fn skill_auto_route_attempt_key(query: &str, skill_name: &str) -> String {
    let mut normalized_skill = String::with_capacity(skill_name.len());
    let mut last_was_dash = true;
    for ch in skill_name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized_skill.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            normalized_skill.push('-');
            last_was_dash = true;
        }
    }
    let normalized_skill = normalized_skill.trim_matches('-');
    let normalized_skill = if normalized_skill.is_empty() {
        "unknown"
    } else {
        normalized_skill
    };
    let mut hasher = Sha256::new();
    hasher.update(query.trim().as_bytes());
    let query_hash = hasher.finalize();
    format!("{normalized_skill}:{query_hash:x}")
}

#[cfg(test)]
fn pure_user_intent_for_runtime_decision(message: &str) -> String {
    message.trim().to_string()
}

async fn maybe_pre_route_skill<H: AgenticLoopHost>(host: &mut H, state: &mut AgenticLoopState) {
    if state.llm_rounds_completed > 0
        || !state.tool_results.is_empty()
        || !state.skills.invoked.is_empty()
    {
        return;
    }

    // Auto-executing a workflow is a control decision, not a catalog hint.
    // Require producer-owned turn semantics before making it. In particular,
    // an asynchronous Work-admission decision must not race a separate skill
    // judge and let a repository skill seize an unrelated user task. The
    // primary model still sees the catalog and may explicitly invoke a skill.
    let Some(turn_intent) = state.turn_intent.as_ref() else {
        return;
    };
    if turn_intent.work_lifecycle == WorkLifecycleIntent::Required {
        return;
    }

    let query = state.runtime_decision_user_intent();
    if query.is_empty() {
        return;
    }

    let Some(resolver) = state.skills.resolver.clone() else {
        return;
    };

    let full = resolver.available_skills();
    if full.is_empty() {
        return;
    }

    let visible =
        crate::turn::skill_tool::visible_skills_for_host_turn(&full, &state.skills.invoked);
    let Some(decision) = host
        .judge_skill_auto_route(
            state,
            SkillAutoRouteJudgeContext {
                query: &query,
                visible_skills: &visible,
            },
        )
        .await
    else {
        tracing::debug!(
            query_length = query.len(),
            visible_skill_count = visible.len(),
            "skill auto-route produced no decision; continuing without pre-route"
        );
        return;
    };
    let skill_name = decision.skill_name.trim().to_string();
    let attempt_key = skill_auto_route_attempt_key(&query, &skill_name);
    if state.skills.auto_route_attempts.contains(&attempt_key) {
        tracing::debug!(
            skill_name,
            "skill auto-route skipped repeated decision for same user intent"
        );
        return;
    }
    if skill_name.is_empty() || !visible.iter().any(|skill| skill.name == skill_name) {
        state.skills.auto_route_attempts.insert(attempt_key);
        tracing::warn!(
            skill_name,
            "skill auto-route judge returned a skill outside the visible catalog"
        );
        return;
    }

    let composition_ctx = crate::skills::composition::CompositionContext::root();
    let skill_ctx = crate::turn::agentic::tool_interception::build_skill_context(state);
    let result = crate::turn::skill_tool::execute_skill_direct(
        resolver.as_ref(),
        state.skills.executor.as_ref(),
        &skill_name,
        &query,
        Some(&composition_ctx),
        &skill_ctx,
    )
    .await;
    let verified = result
        .verification
        .as_ref()
        .map(|outcome| outcome.all_required_passed)
        .unwrap_or(true);
    if !result.success || !verified {
        state.skills.auto_route_attempts.insert(attempt_key);
        return;
    }

    let tool_call_id = auto_route_tool_call_id(&skill_name);
    let mut skill_result =
        crate::turn::skill_tool::append_skill_loaded_marker(&result.output, &skill_name);
    if let Some(activation) = result.activation {
        crate::turn::agentic::tool_interception::apply_skill_activation(state, activation);
    }
    if let Some(notice) =
        crate::turn::agentic::tool_interception::runtime_tool_allowlist_notice(state)
    {
        skill_result.push_str("\n\n");
        skill_result.push_str(&notice);
    }
    let content_for_model = astra_turn_core::tool_result_sanitize::tool_result_content_for_model(
        crate::turn::skill_tool::SKILL_TOOL_NAME,
        &skill_result,
    );

    state.push_prompt_history_message(serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": tool_call_id,
            "type": "function",
            "function": {
                "name": crate::turn::skill_tool::SKILL_TOOL_NAME,
                "arguments": serde_json::json!({
                    "skill_name": skill_name.clone(),
                    "task": query,
                }).to_string(),
            }
        }]
    }));
    let (tool_msg, tool_result) =
        astra_turn_core::headless_tool_assembly::openai_tool_roundtrip_values(
            &tool_call_id,
            crate::turn::skill_tool::SKILL_TOOL_NAME,
            &content_for_model,
        );
    state.push_prompt_history_message(tool_msg);
    state.tool_results.push(tool_result);
    state.telemetry.all_selected_skills.push(skill_name.clone());
    state.skills.auto_route_attempts.insert(attempt_key);
    let execution_topology =
        crate::turn::skill_tool::declared_execution_topology(resolver.as_ref(), &skill_name);
    state.skills.invoked.insert(
        skill_name.clone(),
        crate::turn::skill_tool::InvokedSkill {
            name: skill_name,
            content: skill_result,
            invoked_at_turn: session_turn_number(state),
            reentry_count: 0,
            execution_topology,
        },
    );
}

fn default_budget_exhaustion_completion_text(state: &AgenticLoopState) -> String {
    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let completed_tool_calls = completed_tool_calls(state);
    let completed_agentic_turns = current_agentic_step(state);
    if completed_tool_calls > 0 {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). {} completed tool call(s) are preserved above.{}]\n",
            completed_agentic_turns, completed_tool_calls, checkpoint_note
        )
    } else {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). Partial progress is preserved.{}]\n",
            completed_agentic_turns, checkpoint_note
        )
    }
}

#[derive(Default)]
struct ParallelAgentSummary {
    label: Option<String>,
    completed_result: Option<String>,
    incomplete_reason: Option<String>,
    control_errors: Vec<String>,
}

struct CompletedParallelAgent {
    label: String,
    result: String,
}

struct UnfinishedParallelAgent {
    agent_id: String,
    label: String,
    incomplete_reason: Option<String>,
    control_errors: Vec<String>,
}

struct ParallelAgentBudgetRollup {
    completed: Vec<CompletedParallelAgent>,
    unfinished: Vec<UnfinishedParallelAgent>,
}

fn collect_parallel_agent_budget_rollup(
    state: &AgenticLoopState,
) -> Option<ParallelAgentBudgetRollup> {
    let mut summaries: BTreeMap<String, ParallelAgentSummary> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();

    for record in &state.stall.tool_call_records {
        if record.name != "agent" {
            continue;
        }
        let projection = project_agent_tool_budget_record(record);

        match projection.action {
            AgentToolRecordActionKind::Spawn => {
                let Some(agent_id) = projection.agent_id.clone() else {
                    continue;
                };
                if !order.iter().any(|id| id == &agent_id) {
                    order.push(agent_id.clone());
                }
                let entry = summaries.entry(agent_id).or_default();
                if entry.label.is_none() {
                    entry.label = projection.display_name_hint.clone();
                }
            }
            AgentToolRecordActionKind::GetResult => {
                let Some(agent_id) = projection.agent_id.clone() else {
                    continue;
                };
                if !order.iter().any(|id| id == &agent_id) {
                    order.push(agent_id.clone());
                }
                let entry = summaries.entry(agent_id).or_default();
                if let Some(summarized) = projection.control_error_summary.clone() {
                    if !entry
                        .control_errors
                        .iter()
                        .any(|existing| existing == &summarized)
                    {
                        entry.control_errors.push(summarized);
                    }
                }
                if let Some(result) = projection.completed_result.clone() {
                    entry.completed_result = Some(result);
                } else if entry.completed_result.is_none() {
                    entry.incomplete_reason = projection.incomplete_reason.clone();
                }
            }
            AgentToolRecordActionKind::Other => {}
        }
    }

    let completed: Vec<_> = order
        .iter()
        .filter_map(|agent_id| {
            summaries.get(agent_id).and_then(|entry| {
                entry
                    .completed_result
                    .as_ref()
                    .map(|result| CompletedParallelAgent {
                        label: entry.label.clone().unwrap_or_else(|| agent_id.clone()),
                        result: result.clone(),
                    })
            })
        })
        .collect();
    let unfinished: Vec<_> = order
        .iter()
        .filter_map(|agent_id| {
            summaries.get(agent_id).and_then(|entry| {
                if entry.completed_result.is_some() {
                    return None;
                }
                Some(UnfinishedParallelAgent {
                    agent_id: agent_id.clone(),
                    label: entry.label.clone().unwrap_or_else(|| agent_id.clone()),
                    incomplete_reason: entry.incomplete_reason.clone(),
                    control_errors: entry.control_errors.clone(),
                })
            })
        })
        .collect();

    if completed.is_empty() && unfinished.is_empty() {
        return None;
    }

    Some(ParallelAgentBudgetRollup {
        completed,
        unfinished,
    })
}

fn parallel_agent_budget_exhaustion_summary(
    state: &AgenticLoopState,
    cancelled_agents: &HashSet<String>,
) -> Option<String> {
    let rollup = collect_parallel_agent_budget_rollup(state)?;
    if rollup.completed.is_empty() || rollup.unfinished.is_empty() {
        return None;
    }

    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let mut lines = vec![
        format!(
            "[The owner turn reached its execution boundary after {} agentic turn(s). {} parallel sub-agent result(s) completed; {} did not finish before the turn ended.{}]",
            current_agentic_step(state),
            rollup.completed.len(),
            rollup.unfinished.len(),
            checkpoint_note
        ),
        String::new(),
        "Completed sub-agent results:".to_string(),
    ];
    for (idx, entry) in rollup.completed.iter().enumerate() {
        lines.push(format!(
            "{}. {} — {}",
            idx + 1,
            entry.label,
            summarize_agent_tool_budget_result(&entry.result)
        ));
    }
    lines.push(String::new());
    lines.push("Unfinished sub-agent results:".to_string());
    for (idx, entry) in rollup.unfinished.iter().enumerate() {
        let detail = render_agent_tool_budget_unfinished_detail(
            entry.incomplete_reason.as_deref(),
            &entry.control_errors,
            cancelled_agents.contains(&entry.agent_id),
        );
        lines.push(format!("{}. {} — {}", idx + 1, entry.label, detail));
    }
    Some(lines.join("\n"))
}

pub(crate) fn unfinished_parallel_agent_ids(state: &AgenticLoopState) -> Vec<String> {
    collect_parallel_agent_budget_rollup(state)
        .map(|rollup| {
            rollup
                .unfinished
                .into_iter()
                .map(|entry| entry.agent_id)
                .collect()
        })
        .unwrap_or_default()
}

fn budget_exhaustion_completion_text(
    state: &AgenticLoopState,
    cancelled_agents: &HashSet<String>,
) -> String {
    parallel_agent_budget_exhaustion_summary(state, cancelled_agents)
        .unwrap_or_else(|| default_budget_exhaustion_completion_text(state))
}

async fn cancel_child_agents_with_timeout<H: AgenticLoopHost>(
    host: &mut H,
    runtime_tool_executor: Option<
        std::sync::Arc<crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    >,
    agent_ids: Vec<String>,
    reason: &str,
    origin: CancellationOrigin,
) -> HashSet<String> {
    if agent_ids.is_empty() && runtime_tool_executor.is_none() {
        return HashSet::new();
    }
    let cancellation = async {
        if let Some(executor) = runtime_tool_executor
            && let Some(cancelled) = executor
                .cancel_child_agents(&agent_ids, reason, origin)
                .await
        {
            return cancelled;
        }
        host.cancel_child_agents(&agent_ids, reason, origin).await
    };
    match tokio::time::timeout(CHILD_AGENT_CANCEL_TIMEOUT, cancellation).await {
        Ok(cancelled) => cancelled.into_iter().collect(),
        Err(_) => {
            tracing::warn!(
                target: "astra_runtime::agentic_loop_lifecycle",
                agent_count = agent_ids.len(),
                timeout_ms = CHILD_AGENT_CANCEL_TIMEOUT.as_millis() as u64,
                "timed out while cancelling unfinished child agents"
            );
            HashSet::new()
        }
    }
}

pub(super) async fn cancel_unfinished_child_agents<H: AgenticLoopHost>(
    host: &mut H,
    state: &AgenticLoopState,
    reason: &str,
    origin: CancellationOrigin,
) -> HashSet<String> {
    let execution_lease_lost = state
        .cancellation
        .execution_lease_lost
        .as_ref()
        .is_some_and(|lost| lost.load(Ordering::Acquire));
    if execution_lease_lost && origin != CancellationOrigin::User {
        // A fenced executor no longer owns run-id-scoped descendant control:
        // those identities may already name a newer durable generation. Its
        // inherited token tree still stops local producers, while each child
        // settles through its own generation fence. A canonical user request
        // remains lineage authority and is the sole exception.
        return HashSet::new();
    }
    cancel_child_agents_with_timeout(
        host,
        state.runtime_tool_executor.clone(),
        unfinished_parallel_agent_ids(state),
        reason,
        origin,
    )
    .await
}

pub(crate) async fn resolve_cancellation_origin(
    state: &mut AgenticLoopState,
) -> CancellationOrigin {
    // User is the only run-tree cancellation authority and is monotonic once
    // observed. Runtime/Unverified are provisional execution-local
    // classifications: a durable User marker may linearize after the local
    // token fired but before the terminal CAS, so those values must be
    // reconciled again at settlement.
    if state.cancellation.resolved_origin == Some(CancellationOrigin::User) {
        return CancellationOrigin::User;
    }
    let provisional_origin = state.cancellation.resolved_origin;
    let (Some(run_control), Some(user_id), Some(run_id)) = (
        state.run_control.as_ref(),
        state.context_manifest_user_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        // Without a durable origin provider, a lost lease or other local
        // token-only cancellation is runtime-owned: no user-request fact
        // exists.
        let origin = provisional_origin.unwrap_or(CancellationOrigin::Runtime);
        state.cancellation.resolved_origin = Some(origin);
        return origin;
    };
    let origin = match tokio::time::timeout(
        crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT,
        run_control.cancellation_origin(user_id, run_id),
    )
    .await
    {
        Ok(Ok(origin)) => origin,
        Ok(Err(error)) => {
            tracing::warn!(
                %run_id,
                %error,
                "could not prove agentic-loop cancellation origin"
            );
            provisional_origin.unwrap_or(CancellationOrigin::Unverified)
        }
        Err(_) => {
            tracing::warn!(
                %run_id,
                timeout_ms = crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT
                    .as_millis() as u64,
                "timed out proving agentic-loop cancellation origin"
            );
            provisional_origin.unwrap_or(CancellationOrigin::Unverified)
        }
    };
    state.cancellation.resolved_origin = Some(origin);
    origin
}

async fn finish_cancellation<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    origin: CancellationOrigin,
) -> PreparedTurnIteration {
    let reason = match origin {
        CancellationOrigin::User => "parent turn cancelled by user",
        CancellationOrigin::Runtime => "parent execution cancelled by runtime",
        CancellationOrigin::Unverified => CANCELLATION_ORIGIN_UNVERIFIED,
    };
    let _cancelled = cancel_unfinished_child_agents(host, state, reason, origin).await;
    try_write_heavy_checkpoint(state);
    if origin == CancellationOrigin::User {
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::UserCancelled,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, None),
        ));
    }
    PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
}

fn used_budget_extensions(state: &AgenticLoopState) -> u32 {
    let budget = state.agentic_turn_budget;
    if budget.extension_turns == 0 || state.max_turns <= budget.initial_turns {
        return 0;
    }
    state
        .max_turns
        .saturating_sub(budget.initial_turns)
        .div_ceil(budget.extension_turns)
        .min(u32::MAX as usize) as u32
}

pub(crate) fn extract_tool_args(args: Option<&str>) -> Option<Value> {
    let args = args?;
    serde_json::from_str::<Value>(args).ok()
}

pub(crate) fn tool_record_is_workspace_mutation(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    // Structured writer tools run inside the executor that owns the bound
    // workspace.  Accept only their exact typed receipt; a model/MCP payload
    // that merely resembles this shape is not evidence.
    if record_has_typed_workspace_tool_receipt(record) {
        return true;
    }
    // Prefer an executor-owned post-execution fact over a lexical guess.  The
    // receipt is accepted only from the builtin bash route and only when it
    // is bound to this workspace; arbitrary tool metadata must not satisfy a
    // workspace completion contract.
    if record.name == "bash"
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
        && record
            .workspace_mutation_receipt
            .as_ref()
            .is_some_and(astra_tools::workspace_observation::is_changed_receipt)
    {
        return true;
    }
    // `args_preview` is display-only and may be truncated or stale.  It must
    // never become historical proof that an argument-dependent tool mutated
    // the workspace; direct typed writers are handled by their tool identity
    // below, while Bash/Git require the lossless args_full contract (or the
    // executor-owned receipt above).
    let args = extract_tool_args(record.authoritative_args_full());
    // Live admission treats missing arguments conservatively because safety
    // must fail closed. Historical completion evidence has the opposite
    // burden: malformed journal arguments are not proof that a mutation
    // actually happened and must not satisfy a requested-change contract.
    if args.is_none()
        && crate::turn::tool_side_effects::tool_classified_from_arguments(&record.name)
    {
        return false;
    }
    crate::turn::tool_side_effects::tool_call_records_workspace_mutation(
        &record.name,
        args.as_ref(),
    )
}

pub(crate) fn record_has_typed_workspace_tool_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.was_executed()
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
        && astra_tools::workspace_observation::is_typed_workspace_tool_receipt(
            record
                .workspace_mutation_receipt
                .as_ref()
                .unwrap_or(&Value::Null),
        )
        && astra_tools::executor::is_workspace_mutation_tool(
            &record.name,
            &extract_tool_args(record.authoritative_args_full()).unwrap_or(Value::Null),
        )
}

pub(crate) fn record_has_typed_workspace_observation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let args = extract_tool_args(record.authoritative_args_full()).unwrap_or(Value::Null);
    let receipt = record
        .workspace_mutation_receipt
        .as_ref()
        .unwrap_or(&Value::Null);
    record.was_executed()
        && record.ok
        && ((astra_tools::workspace_observation::is_typed_workspace_observer(&record.name)
            && astra_tools::workspace_observation::is_typed_workspace_observation_receipt(receipt))
            || (astra_tools::workspace_observation::is_explicit_workspace_verification_request(
                &record.name,
                &args,
            )
                && astra_tools::workspace_observation::is_explicit_workspace_verification_receipt(
                    receipt,
                )))
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
}

fn recent_turns_are_repetitive(state: &AgenticLoopState) -> bool {
    let Some(last) = state.stall.turn_sigs.last() else {
        return false;
    };
    if last.is_empty() {
        return false;
    }
    state
        .stall
        .turn_sigs
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|previous| previous == last)
}

fn record_failure_or_rejection(record: &astra_services::session_journal::ToolCallRecord) -> bool {
    record.was_executed() && !record.ok
        || matches!(
            record.disposition,
            Some(astra_services::session_journal::ToolCallDisposition::Rejected)
        )
}

pub(crate) fn record_explicit_path(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<String> {
    // A live record has an executor-owned, lossless argument lane.  Prefer it
    // over `file_path`, which is a display projection and may itself contain
    // a redaction marker when the filename resembles a credential.  If the
    // live arguments are present but do not expose a direct path, do not fall
    // back to the projection and accidentally compare a different target.
    if let Some(args) = extract_tool_args(record.authoritative_args_full()) {
        let path = ["path", "file_path", "notebook_path", "destination", "dest"]
            .into_iter()
            .find_map(|key| {
                args.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(ToString::to_string)
            });
        if path.is_some() || record.runtime_args_full.is_some() {
            return path;
        }
    }
    if record.runtime_args_full.is_some() {
        return None;
    }
    // `args_preview` is an audit display field and is intentionally truncated
    // by the journal.  It is never an execution contract, even when a short
    // preview happens to be valid JSON.  A durable `file_path` is acceptable
    // only when it is not visibly redacted; otherwise this helper fails
    // closed rather than treating a marker as a real filesystem path.
    record
        .file_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty() && !path.contains("[REDACTED:"))
        .map(ToString::to_string)
}

fn normalize_absolute_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

/// A shell-expanded path is not a durable scope receipt.  Resolving it as a
/// literal relative path can accidentally place an external `$TMPDIR`/glob
/// under the bound workspace and promote unrelated validation to success.
fn path_contains_unresolved_shell_expansion(path: &str) -> bool {
    path.chars()
        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | ']'))
}

/// Resolve an evidence path against the bound workspace without consulting
/// the process cwd.  A relative path is meaningful only when the executor
/// supplied a workspace binding; otherwise it is intentionally not a
/// scheduler receipt.
pub(crate) fn normalize_workspace_path(
    path: &str,
    workspace_root: Option<&str>,
) -> Option<PathBuf> {
    if path_contains_unresolved_shell_expansion(path) {
        return None;
    }
    let root = workspace_root.and_then(normalize_absolute_path)?;
    let raw = Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let candidate = normalize_absolute_path(candidate.to_str()?)?;
    candidate.starts_with(&root).then_some(candidate)
}

fn normalize_scoped_path(path: &str, cwd: &Path, root: &Path) -> Option<PathBuf> {
    if path_contains_unresolved_shell_expansion(path) {
        return None;
    }
    let raw = Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let candidate = normalize_absolute_path(candidate.to_str()?)?;
    candidate.starts_with(root).then_some(candidate)
}

fn scope_token_is_path_like(token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token == "."
        || token == ".."
        || (token.contains('/') && !token.contains("://"))
}

fn scoped_command_path_tokens(words: &[String]) -> Vec<(usize, &str)> {
    let mut indices = Vec::new();
    for (index, word) in words.iter().enumerate().skip(1) {
        let candidate = word
            .split_once('=')
            .map(|(_, value)| value)
            .filter(|_| word.starts_with('-'))
            .unwrap_or(word);
        if !candidate.starts_with('-') && scope_token_is_path_like(candidate) {
            indices.push((index, candidate));
        }
    }
    indices
}

/// Prove that a shell validator/read observes the bound workspace rather
/// than an unrelated checkout. This is evidence classification only: shell
/// execution and permission policy remain independent. Unknown syntax or
/// dynamic scope is intentionally not a receipt when a workspace binding is
/// available.
pub(crate) fn bash_command_is_workspace_scoped(
    command: &str,
    workspace_root: Option<&str>,
) -> bool {
    let Some(root) = workspace_root.and_then(normalize_absolute_path) else {
        // Without a binding there is no safe scope to compare against; keep
        // legacy receipt behavior and let the executor/permission layer own
        // isolation.
        return true;
    };
    let Some(segments) = astra_turn_core::evaluation::split_shell_control_segments(command) else {
        return false;
    };
    let mut cwd = root.clone();
    for segment in segments
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let Some(pipeline_segments) =
            astra_turn_core::evaluation::split_shell_pipeline_segments(segment)
        else {
            return false;
        };
        for pipeline_segment in pipeline_segments {
            let pipeline_segment = pipeline_segment.trim();
            if pipeline_segment.is_empty() {
                continue;
            }
            let pipeline_segment = pipeline_segment
                .strip_suffix("2>&1")
                .or_else(|| pipeline_segment.strip_suffix("1>&2"))
                .map(str::trim)
                .unwrap_or(pipeline_segment);
            let Some(words) = shell_file_test_words(pipeline_segment) else {
                // Dynamic cwd, manifest, or path expressions cannot be
                // correlated to this workspace. Do not retain an older cwd
                // and accidentally turn an unrelated validation into proof.
                return false;
            };
            let Some(head) = words.first() else { continue };
            let head_lower = head.to_ascii_lowercase();
            // A control wrapper is only neutral when it has no state-changing
            // shell syntax. Check redirects before handling `cd`/`pushd`/`set`;
            // a redirected control segment can write concurrently with a
            // later validator in the same pipeline. Ordinary writer redirects
            // remain in scope here; the receipt classifier handles their
            // ordering separately.
            if matches!(head_lower.as_str(), "cd" | "pushd" | "set")
                && shell_segment_has_non_benign_redirect(pipeline_segment)
            {
                return false;
            }
            if matches!(head_lower.as_str(), "cd" | "pushd") {
                let Some(target) = words.iter().skip(1).find(|word| !word.starts_with('-')) else {
                    return false;
                };
                let Some(next_cwd) = normalize_scoped_path(target, &cwd, &root) else {
                    return false;
                };
                cwd = next_cwd;
                continue;
            }
            if head_lower == "popd" {
                return false;
            }
            for (_, token) in scoped_command_path_tokens(&words) {
                if normalize_scoped_path(token, &cwd, &root).is_none() {
                    return false;
                }
            }
        }
    }
    true
}

/// A read-only shell command is not automatically a workspace observation:
/// `echo`, `true`, and `sleep` are safe but carry no state receipt. Keep this
/// positive capability separate from permission classification.
pub(crate) fn bash_command_has_workspace_observation_shape(command: &str) -> bool {
    let Some(segments) =
        astra_turn_core::evaluation::split_shell_control_segments_with_ops(command)
    else {
        return false;
    };
    let mut observed = false;
    let mut unobserved_opaque_mutation = false;
    for (segment_index, (raw_segment, op_after)) in segments.iter().enumerate() {
        let segment = raw_segment.trim();
        if !segment.is_empty() && !segment.starts_with('#') {
            let positive = shell_segment_has_positive_observation(segment);
            if positive {
                let literal_script = shell_segment_contains_literal_script_artifact(segment);
                observed = !literal_script || !unobserved_opaque_mutation;
                if observed {
                    unobserved_opaque_mutation = false;
                }
            } else if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment) {
                // A successful observer only describes the state at its own
                // ordered boundary. An opaque writer on the same successful
                // shell path reopens the mutation epoch even when joined by
                // `&&` (for example `check.py && touch stamp`).
                observed = false;
                unobserved_opaque_mutation = true;
            }
        }

        // A reader/validator followed by `||` or by a real sequence RHS does
        // not prove the overall command: the reader may have failed and the
        // later branch can still make the shell return success. `&&` keeps
        // the receipt on the successful path. A later positive segment may
        // establish a fresh receipt after a barrier.
        let sequence_has_rhs = segments[segment_index + 1..].iter().any(|(rhs, _)| {
            let rhs = rhs.trim();
            !rhs.is_empty() && !rhs.starts_with('#')
        });
        if matches!(op_after, astra_turn_core::evaluation::ShellControlOp::Or)
            || (matches!(
                op_after,
                astra_turn_core::evaluation::ShellControlOp::Sequence
            ) && sequence_has_rhs)
        {
            observed = false;
        }
    }
    observed
}

/// Prove that the command has a surviving positive observation of the bound
/// workspace. Earlier ordered segments may legitimately use external scratch
/// paths (for example a test log under `/tmp`); only the segment carrying the
/// surviving receipt must be workspace-scoped. Cwd transitions remain
/// authoritative, and unknown/concurrent syntax fails closed.
pub(crate) fn bash_command_has_scoped_workspace_observation(
    command: &str,
    workspace_root: Option<&str>,
) -> bool {
    let Some(root) = workspace_root.and_then(normalize_absolute_path) else {
        return bash_command_has_workspace_observation_shape(command);
    };
    let Some(segments) =
        astra_turn_core::evaluation::split_shell_control_segments_with_ops(command)
    else {
        return false;
    };
    let mut cwd = Some(root.clone());
    let mut observed = false;
    let mut unobserved_opaque_mutation = false;
    for (segment_index, (raw_segment, op_after)) in segments.iter().enumerate() {
        let segment = raw_segment.trim();
        if !segment.is_empty() && !segment.starts_with('#') {
            let positive = shell_segment_has_positive_observation(segment);
            if positive {
                let literal_script = shell_segment_contains_literal_script_artifact(segment);
                observed = (!literal_script || !unobserved_opaque_mutation)
                    && cwd.as_ref().is_some_and(|cwd| {
                        shell_observation_segment_is_workspace_scoped(segment, cwd, &root)
                    });
                if observed {
                    unobserved_opaque_mutation = false;
                }
            } else if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment) {
                observed = false;
                unobserved_opaque_mutation = true;
            }
            apply_ordered_shell_cwd_transition(segment, &mut cwd);
        }

        let sequence_has_rhs = segments[segment_index + 1..].iter().any(|(rhs, _)| {
            let rhs = rhs.trim();
            !rhs.is_empty() && !rhs.starts_with('#')
        });
        if matches!(op_after, astra_turn_core::evaluation::ShellControlOp::Or)
            || (matches!(
                op_after,
                astra_turn_core::evaluation::ShellControlOp::Sequence
            ) && sequence_has_rhs)
        {
            observed = false;
        }
    }
    observed
}

/// Whether the command's surviving receipt is specifically a literal script
/// execution. This syntactic family marker is intentionally independent of a
/// workspace binding so callers can fail closed when the exact target cannot
/// be normalized, instead of silently treating it as a canonical validator.
pub(crate) fn bash_command_has_literal_script_artifact_observation_shape(command: &str) -> bool {
    let Some(segments) =
        astra_turn_core::evaluation::split_shell_control_segments_with_ops(command)
    else {
        return false;
    };
    let mut artifact_family = false;
    for (segment_index, (raw_segment, op_after)) in segments.iter().enumerate() {
        let segment = raw_segment.trim();
        if !segment.is_empty() && !segment.starts_with('#') {
            if shell_segment_has_positive_observation(segment) {
                artifact_family = shell_segment_contains_literal_script_artifact(segment);
            } else if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment) {
                artifact_family = false;
            }
        }

        let sequence_has_rhs = segments[segment_index + 1..].iter().any(|(rhs, _)| {
            let rhs = rhs.trim();
            !rhs.is_empty() && !rhs.starts_with('#')
        });
        if matches!(op_after, astra_turn_core::evaluation::ShellControlOp::Or)
            || (matches!(
                op_after,
                astra_turn_core::evaluation::ShellControlOp::Sequence
            ) && sequence_has_rhs)
        {
            artifact_family = false;
        }
    }
    artifact_family
}

/// Return the exact bound-workspace artifact observed by the surviving
/// literal-script receipt. A later opaque writer clears the receipt, and a
/// canonical validator is deliberately not projected as artifact identity.
/// Requiring a concrete normalized target lets the completion layer correlate
/// behavior with an executor-owned structured delivery instead of accepting
/// an arbitrary historical mutation.
pub(crate) fn bash_literal_script_artifact_observation_target(
    command: &str,
    workspace_root: Option<&str>,
) -> Option<PathBuf> {
    if !bash_command_has_literal_script_artifact_observation_shape(command) {
        return None;
    }
    let root = workspace_root.and_then(normalize_absolute_path)?;
    let segments = astra_turn_core::evaluation::split_shell_control_segments_with_ops(command)?;
    let mut cwd = Some(root.clone());
    let mut artifact = None;
    let mut other_mutation = false;
    for (segment_index, (raw_segment, op_after)) in segments.iter().enumerate() {
        let segment = raw_segment.trim();
        if !segment.is_empty() && !segment.starts_with('#') {
            let positive = shell_segment_has_positive_observation(segment);
            if positive {
                artifact = cwd
                    .as_ref()
                    .filter(|cwd| {
                        shell_observation_segment_is_workspace_scoped(segment, cwd, &root)
                    })
                    .and_then(|cwd| {
                        shell_segment_literal_script_artifact(segment)
                            .and_then(|path| normalize_scoped_path(&path, cwd, &root))
                    });
            } else if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment) {
                // The owner receipt spans the whole shell invocation. Do not
                // attribute any other opaque writer, before or after the
                // script, to the delivered artifact's behavior.
                other_mutation = true;
                artifact = None;
            }
            apply_ordered_shell_cwd_transition(segment, &mut cwd);
        }

        let sequence_has_rhs = segments[segment_index + 1..].iter().any(|(rhs, _)| {
            let rhs = rhs.trim();
            !rhs.is_empty() && !rhs.starts_with('#')
        });
        if matches!(op_after, astra_turn_core::evaluation::ShellControlOp::Or)
            || (matches!(
                op_after,
                astra_turn_core::evaluation::ShellControlOp::Sequence
            ) && sequence_has_rhs)
        {
            artifact = None;
        }
    }
    (!other_mutation).then_some(artifact).flatten()
}

fn shell_observation_segment_is_workspace_scoped(segment: &str, cwd: &Path, root: &Path) -> bool {
    if !cwd.starts_with(root) {
        return false;
    }
    let Some(pipeline_segments) =
        astra_turn_core::evaluation::split_shell_pipeline_segments(segment)
    else {
        return false;
    };
    for raw_pipeline in pipeline_segments {
        let pipeline = raw_pipeline
            .trim()
            .strip_suffix("2>&1")
            .or_else(|| raw_pipeline.trim().strip_suffix("1>&2"))
            .map(str::trim)
            .unwrap_or_else(|| raw_pipeline.trim());
        let Some(words) = shell_file_test_words(pipeline) else {
            return false;
        };
        for (_, token) in scoped_command_path_tokens(&words) {
            if normalize_scoped_path(token, cwd, root).is_none() {
                return false;
            }
        }
    }
    true
}

fn apply_ordered_shell_cwd_transition(segment: &str, cwd: &mut Option<PathBuf>) {
    let Some(pipeline_segments) =
        astra_turn_core::evaluation::split_shell_pipeline_segments(segment)
    else {
        *cwd = None;
        return;
    };
    // A cwd-changing builtin inside a pipeline does not establish a
    // deterministic parent-shell cwd for later evidence.
    if pipeline_segments.len() != 1 {
        return;
    }
    let pipeline = pipeline_segments[0].trim();
    let Some(words) = shell_file_test_words(pipeline) else {
        let head = pipeline
            .split_whitespace()
            .next()
            .map(|word| word.trim_matches(['\'', '"']).to_ascii_lowercase());
        if matches!(head.as_deref(), Some("cd" | "pushd")) {
            *cwd = None;
        }
        return;
    };
    let Some(head) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return;
    };
    if head == "popd" {
        *cwd = None;
        return;
    }
    if !matches!(head.as_str(), "cd" | "pushd") {
        return;
    }
    let Some(target) = words.iter().skip(1).find(|word| !word.starts_with('-')) else {
        *cwd = None;
        return;
    };
    if path_contains_unresolved_shell_expansion(target) {
        *cwd = None;
        return;
    }
    let raw = Path::new(target);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else if let Some(current) = cwd.as_ref() {
        current.join(raw)
    } else {
        *cwd = None;
        return;
    };
    *cwd = candidate.to_str().and_then(normalize_absolute_path);
}

fn shell_segment_has_positive_observation(segment: &str) -> bool {
    if let Some(words) = shell_file_test_words(segment) {
        let head = words
            .first()
            .map(|word| word.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(head.as_str(), "test" | "[") {
            return shell_file_test_has_workspace_operand(&words);
        }
    }
    let Some(pipelines) = astra_turn_core::evaluation::split_shell_pipeline_segments(segment)
    else {
        return false;
    };
    // A downstream pipeline stage can terminate early (`head`) or otherwise
    // mask the interpreter's status when `pipefail` is not active. Therefore
    // a literal script is behavioral evidence only as a single foreground
    // command; never let a pipeline containing it fall through to the generic
    // reader/validator provenance rules.
    if pipelines.len() > 1
        && pipelines.iter().any(|raw_pipeline| {
            let pipeline = raw_pipeline
                .trim()
                .strip_suffix("2>&1")
                .or_else(|| raw_pipeline.trim().strip_suffix("1>&2"))
                .map(str::trim)
                .unwrap_or_else(|| raw_pipeline.trim());
            shell_pipeline_executes_literal_script_artifact(pipeline)
        })
    {
        return false;
    }
    // A pipeline is concurrent, so an unknown or mutating stage invalidates
    // the whole receipt group. A later validator cannot prove ordering around
    // `touch`, an inline writer, or another opaque stage. Known read-only
    // stages and canonical validators remain eligible for the provenance
    // rules below.
    if pipelines.iter().any(|raw_pipeline| {
        let pipeline = raw_pipeline
            .trim()
            .strip_suffix("2>&1")
            .or_else(|| raw_pipeline.trim().strip_suffix("1>&2"))
            .map(str::trim)
            .unwrap_or_else(|| raw_pipeline.trim());
        shell_segment_has_non_benign_redirect(pipeline)
            || (!astra_turn_core::cloud_approval_policy::bash_command_is_read_only(pipeline)
                && !astra_turn_core::evaluation::bash_command_has_post_mutation_validation(
                    pipeline,
                )
                && !shell_pipeline_executes_literal_script_artifact(pipeline))
    }) {
        return false;
    }
    let mut upstream_workspace_receipt = false;
    for raw_pipeline in &pipelines {
        let pipeline = raw_pipeline
            .trim()
            .strip_suffix("2>&1")
            .or_else(|| raw_pipeline.trim().strip_suffix("1>&2"))
            .map(str::trim)
            .unwrap_or_else(|| raw_pipeline.trim());
        if shell_segment_has_non_benign_redirect(pipeline) {
            return false;
        }
        if shell_segment_has_reader_meta_option(pipeline) {
            return false;
        }
        let Some(words) = shell_file_test_words(pipeline) else {
            // The canonical validator may still understand a compound
            // pipeline, but an unknown reader shape is not a receipt by
            // itself. Keep the positive classifier fail-closed.
            continue;
        };
        let Some(head) = words.first().map(|word| word.to_ascii_lowercase()) else {
            continue;
        };
        let basic_receipt = shell_segment_has_basic_workspace_observation(pipeline);
        // The broad core predicate intentionally accepts generic local checks
        // such as `test` as a receipt for a whole command. It is not safe to
        // use that broad result as pipeline provenance: `test foo` and
        // `test foo | cat` inspect only literals/stdin. Pipeline stages may
        // inherit only from a strict canonical validator, or from the basic
        // operand-aware receipt calculated above.
        let literal_test_without_operand =
            matches!(head.as_str(), "test" | "[") && !shell_file_test_has_workspace_operand(&words);
        let canonical_receipt =
            astra_turn_core::evaluation::bash_command_post_mutation_validation_prefix(pipeline)
                .is_some()
                || (!literal_test_without_operand
                    && astra_turn_core::evaluation::bash_command_has_post_mutation_validation(
                        pipeline,
                    ))
                || shell_pipeline_executes_literal_script_artifact(pipeline);
        // A reader without an operand may consume a previous pipeline stage
        // (e.g. `cargo test | tail -20`), but it may only inherit a receipt
        // from a prior stage that itself has a workspace operand or a
        // canonical validation shape. Pipeline position alone is not proof:
        // `printf x | cat` and `[ foo ] | cat` only process literals/stdin.
        if shell_reader_requires_operand_check(&head)
            && !basic_receipt
            && !upstream_workspace_receipt
        {
            return false;
        }
        if basic_receipt || canonical_receipt {
            upstream_workspace_receipt = true;
        }
    }
    upstream_workspace_receipt
        || astra_turn_core::evaluation::bash_command_post_mutation_validation_prefix(segment)
            .is_some()
        || (pipelines.len() == 1
            && astra_turn_core::evaluation::bash_command_has_post_mutation_validation(segment))
}

/// A successful foreground execution of a literal script in the bound
/// workspace is a direct behavioral observation of that delivered artifact.
/// This is intentionally narrower than treating an arbitrary interpreter
/// invocation or exit code as evidence: inline code, modules, stdin, dynamic
/// paths, and mismatched extensions remain opaque. The caller separately
/// proves the ordered cwd/path stays inside the bound workspace.
fn shell_segment_contains_literal_script_artifact(segment: &str) -> bool {
    astra_turn_core::evaluation::split_shell_pipeline_segments(segment).is_some_and(|pipelines| {
        pipelines.iter().any(|pipeline| {
            shell_pipeline_literal_script_artifact(
                pipeline
                    .trim()
                    .strip_suffix("2>&1")
                    .or_else(|| pipeline.trim().strip_suffix("1>&2"))
                    .map(str::trim)
                    .unwrap_or_else(|| pipeline.trim()),
            )
            .is_some()
        })
    })
}

fn shell_segment_literal_script_artifact(segment: &str) -> Option<String> {
    let pipelines = astra_turn_core::evaluation::split_shell_pipeline_segments(segment)?;
    let mut artifacts = pipelines.iter().filter_map(|pipeline| {
        shell_pipeline_literal_script_artifact(
            pipeline
                .trim()
                .strip_suffix("2>&1")
                .or_else(|| pipeline.trim().strip_suffix("1>&2"))
                .map(str::trim)
                .unwrap_or_else(|| pipeline.trim()),
        )
    });
    let artifact = artifacts.next()?;
    artifacts.next().is_none().then_some(artifact)
}

fn shell_pipeline_executes_literal_script_artifact(pipeline: &str) -> bool {
    shell_pipeline_literal_script_artifact(pipeline).is_some()
}

fn shell_pipeline_literal_script_artifact(segment: &str) -> Option<String> {
    if shell_segment_has_non_benign_redirect(segment)
        || shell_segment_has_reader_meta_option(segment)
    {
        return None;
    }
    let words = astra_turn_core::evaluation::split_static_shell_words(segment)?;
    let [interpreter, script, ..] = words.as_slice() else {
        return None;
    };
    if script.starts_with('-') || matches!(script.as_str(), "-" | "." | "..") {
        return None;
    }
    let interpreter = Path::new(interpreter)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)?;
    let extension = Path::new(script)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let extension_matches = match interpreter.as_str() {
        name if name == "python"
            || name == "python3"
            || name.strip_prefix("python3.").is_some_and(|minor| {
                !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit())
            }) =>
        {
            extension.as_deref() == Some("py")
        }
        "node" | "nodejs" => matches!(extension.as_deref(), Some("js" | "mjs" | "cjs")),
        "ruby" => extension.as_deref() == Some("rb"),
        "perl" => extension.as_deref() == Some("pl"),
        "php" => extension.as_deref() == Some("php"),
        "bash" | "sh" | "zsh" => extension.as_deref() == Some("sh"),
        _ => false,
    };
    extension_matches.then(|| script.clone())
}

fn shell_reader_requires_operand_check(head: &str) -> bool {
    matches!(
        head,
        "cat"
            | "head"
            | "tail"
            | "grep"
            | "rg"
            | "ls"
            | "find"
            | "file"
            | "stat"
            | "readlink"
            | "wc"
            | "sed"
            | "cmp"
            | "diff"
            | "sha256sum"
            | "sha512sum"
            | "git"
    )
}

fn shell_segment_has_non_benign_redirect(segment: &str) -> bool {
    let segment = segment
        .trim()
        .strip_suffix("2>&1")
        .or_else(|| segment.trim().strip_suffix("1>&2"))
        .map(str::trim)
        .unwrap_or_else(|| segment.trim());
    // Keep redirect syntax classification on the shared raw shell scanner.
    // The lifecycle path must agree with core for quoted literals such as
    // `grep '>' file`; tokenizing first would turn that literal into a false
    // output redirect and reject valid observation evidence.
    astra_turn_core::evaluation::shell_segment_has_non_benign_redirect(segment)
}

fn shell_file_test_words(segment: &str) -> Option<Vec<String>> {
    let trimmed = segment.trim();
    if trimmed.starts_with('[') {
        let body = trimmed.strip_prefix('[')?.strip_suffix(']')?.trim();
        return astra_turn_core::evaluation::split_static_shell_words(&format!("test {body}"));
    }
    astra_turn_core::evaluation::split_static_shell_words(trimmed)
}

fn shell_segment_has_reader_meta_option(segment: &str) -> bool {
    let segment = segment
        .trim()
        .strip_suffix("2>&1")
        .or_else(|| segment.trim().strip_suffix("1>&2"))
        .map(str::trim)
        .unwrap_or_else(|| segment.trim());
    let Some(words) = astra_turn_core::evaluation::split_static_shell_words(segment) else {
        return false;
    };
    let head = words
        .first()
        .map(|word| word.to_ascii_lowercase())
        .unwrap_or_default();
    let mut options_done = false;
    words.iter().skip(1).any(|word| {
        if options_done {
            return false;
        }
        if word == "--" {
            options_done = true;
            return false;
        }
        matches!(word.as_str(), "-V" | "-?")
            || (word == "-h"
                && matches!(
                    head.as_str(),
                    "git" | "cat" | "head" | "tail" | "sed" | "cargo" | "python" | "python3"
                ))
            || matches!(
                word.to_ascii_lowercase().as_str(),
                "--help" | "--version" | "--usage"
            )
    })
}

fn shell_segment_has_basic_workspace_observation(segment: &str) -> bool {
    let Some(pipelines) = astra_turn_core::evaluation::split_shell_pipeline_segments(segment)
    else {
        return false;
    };
    pipelines.into_iter().any(|pipeline| {
        let pipeline = pipeline
            .trim()
            .strip_suffix("2>&1")
            .or_else(|| pipeline.trim().strip_suffix("1>&2"))
            .map(str::trim)
            .unwrap_or_else(|| pipeline.trim());
        if shell_segment_has_non_benign_redirect(pipeline) {
            return false;
        }
        let Some(words) = shell_file_test_words(pipeline) else {
            return false;
        };
        let Some(head) = words.first().map(|word| word.to_ascii_lowercase()) else {
            return false;
        };
        if shell_segment_has_reader_meta_option(pipeline) {
            return false;
        }
        match head.as_str() {
            "git" => shell_git_has_workspace_subcommand(&words),
            "ls" | "find" => true,
            "grep" | "rg" => shell_grep_has_workspace_operand(&words, &head),
            "sed" => {
                shell_reader_positional_count(&words, &head) >= 1
                    && words
                        .iter()
                        .skip(1)
                        .any(|word| word == "-n" || word == "--quiet")
            }
            "cat" | "head" | "tail" | "file" | "stat" | "readlink" | "wc" | "cmp" | "diff" => {
                shell_reader_positional_count(&words, &head) >= 1
            }
            "sha256sum" | "sha512sum" => shell_reader_positional_count(&words, &head) >= 1,
            "test" | "[" => shell_file_test_has_workspace_operand(&words),
            _ => false,
        }
    })
}

fn shell_git_has_workspace_subcommand(words: &[String]) -> bool {
    let mut skip_next = false;
    for word in words.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if word == "--" {
            return false;
        }
        if word.starts_with('-') {
            if shell_option_takes_value_for_reader("git", word) {
                skip_next = true;
            }
            continue;
        }
        return matches!(
            word.to_ascii_lowercase().as_str(),
            "status" | "diff" | "show" | "log" | "ls-files" | "branch" | "remote"
        );
    }
    false
}

fn shell_reader_positional_count(words: &[String], head: &str) -> usize {
    let mut count = 0;
    let mut skip_next = false;
    let mut options_done = false;
    for word in words.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if options_done {
            count += 1;
            continue;
        }
        if word == "--" {
            options_done = true;
            continue;
        }
        if word.starts_with('-') {
            if shell_option_takes_value_for_reader(head, word) && !word.contains('=') {
                skip_next = true;
            }
            continue;
        }
        count += 1;
    }
    count
}

fn shell_grep_has_workspace_operand(words: &[String], head: &str) -> bool {
    let positional = shell_reader_positional_count(words, head);
    let pattern_from_option = words.iter().skip(1).any(|word| {
        matches!(word.as_str(), "-e" | "--regexp" | "-f" | "--file")
            || word.starts_with("--regexp=")
            || word.starts_with("--file=")
    });
    if pattern_from_option {
        positional >= 1
    } else {
        positional >= 2
    }
}

fn shell_file_test_has_workspace_operand(words: &[String]) -> bool {
    let mut operands = words
        .iter()
        .skip(1)
        .filter(|word| word.as_str() != "]")
        .filter(|word| !matches!(word.as_str(), "!" | "("))
        .peekable();
    while let Some(word) = operands.next() {
        if matches!(
            word.as_str(),
            "-e" | "-f"
                | "-d"
                | "-L"
                | "-h"
                | "-b"
                | "-c"
                | "-p"
                | "-r"
                | "-w"
                | "-x"
                | "-s"
                | "-O"
                | "-G"
                | "-N"
                | "-ef"
                | "-nt"
                | "-ot"
        ) {
            return operands.next().is_some();
        }
    }
    false
}

fn shell_option_takes_value_for_reader(head: &str, option: &str) -> bool {
    match head {
        "wc" => matches!(option, "--files0-from"),
        "grep" | "rg" => matches!(
            option,
            "-e" | "--regexp"
                | "-f"
                | "--file"
                | "-C"
                | "--context"
                | "--include"
                | "--exclude"
                | "--glob"
                | "-m"
                | "--max-count"
        ),
        "head" | "tail" => matches!(option, "-n" | "--lines" | "-c" | "--bytes"),
        "stat" => matches!(option, "-c" | "--format" | "--printf" | "-t"),
        "find" => matches!(
            option,
            "-maxdepth" | "-mindepth" | "-name" | "-path" | "-type" | "-printf"
        ),
        _ => shell_option_takes_value(option),
    }
}

fn shell_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-n" | "--lines"
            | "-c"
            | "--bytes"
            | "-m"
            | "--max-count"
            | "-e"
            | "--regexp"
            | "-f"
            | "--file"
            | "-C"
            | "--context"
            | "-d"
            | "--delimiter"
            | "-t"
            | "--time-style"
            | "--format"
            | "--max-depth"
            | "--min-depth"
            | "--include"
            | "--exclude"
            | "--glob"
            | "--type"
            | "--color"
            | "--git-dir"
            | "--work-tree"
    )
}

/// Match the same positive, workspace-scoped observation contract used by the
/// executed ledger against a provider call before it is admitted. The raw
/// call has no ToolCallRecord yet, so keep this small adapter next to the
/// record predicate rather than duplicating a weaker may-observe check in the
/// completion-action window.
pub(crate) fn tool_call_can_observe_bound_workspace(
    state: &AgenticLoopState,
    name: &str,
    args: Option<&Value>,
) -> bool {
    let root = state.hooks.workspace_root_hint.as_deref();
    if name == "bash" {
        // This only permits the bounded recovery action to run.  Settlement
        // still requires the executor-produced v2 receipt in the completed
        // record; see `record_can_observe_bound_workspace` below.
        if args.is_some_and(|args| {
            astra_tools::workspace_observation::is_explicit_workspace_verification_request(
                name, args,
            )
        }) {
            return true;
        }
        let Some(command) = args
            .and_then(|args| astra_turn_core::tool_argument_hints::command_hint_from_args(args))
        else {
            return false;
        };
        return bash_command_has_scoped_workspace_observation(command, root);
    }
    if !crate::turn::tool_side_effects::tool_call_may_observe_workspace(name, args) {
        return false;
    }
    let explicit_path = args.and_then(|args| {
        ["path", "file_path", "notebook_path", "destination", "dest"]
            .into_iter()
            .find_map(|key| args.get(key).and_then(Value::as_str))
    });
    explicit_path
        .is_none_or(|path| root.is_none() || normalize_workspace_path(path, root).is_some())
}

/// Scope an executed observation to the bound workspace. Direct read tools
/// use their typed path; Bash uses the shared shell scope classifier.
pub(crate) fn record_can_observe_bound_workspace(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    // The observer ran on the owner side of the bound workspace.  This typed
    // receipt is the only portable way to carry that fact across Edge→server;
    // do not stat or canonicalize the server's unrelated local path.
    if record_has_typed_workspace_observation_receipt(record) {
        return true;
    }
    let root = state.hooks.workspace_root_hint.as_deref();
    let args = extract_tool_args(record.authoritative_args_full());
    if record.name == "bash" {
        // Verify mode is an explicit receipt contract.  Do not silently
        // downgrade a missing/forged v2 receipt to the legacy command-shape
        // classifier, or a cached/model-authored result could clear a
        // post-mutation obligation without a fresh executor observation.
        if args.as_ref().is_some_and(|args| {
            astra_tools::workspace_observation::is_explicit_workspace_verification_request(
                &record.name,
                args,
            )
        }) {
            return false;
        }
        return tool_call_can_observe_bound_workspace(state, &record.name, args.as_ref());
    }
    if !crate::turn::tool_side_effects::tool_call_may_observe_workspace(&record.name, args.as_ref())
    {
        return false;
    }
    if let Some(path) = record_explicit_path(record) {
        return root.is_none() || normalize_workspace_path(&path, root).is_some();
    }
    true
}

pub(crate) fn path_is_external_volatile_scratch(path: &str, workspace_root: Option<&str>) -> bool {
    let Some(path) = normalize_absolute_path(path) else {
        return false;
    };
    let workspace_root = workspace_root
        .and_then(normalize_absolute_path)
        .filter(|root| root != Path::new("/"));
    if workspace_root
        .as_ref()
        .is_some_and(|root| path.starts_with(root))
    {
        return false;
    }
    ["/tmp", "/var/tmp", "/dev/shm"]
        .into_iter()
        .filter_map(normalize_absolute_path)
        .any(|root| path == root || path.starts_with(root))
}

/// Return concrete targets only for a simple, unambiguous shell mutation.
/// Complex control flow, substitutions, pipelines, and heredocs remain
/// unknown here; the command is still executable, but cannot serve as a
/// scheduler renewal receipt without an executor-provided file_path.
fn simple_bash_mutation_targets(command: &str) -> Option<Vec<String>> {
    if command.contains([';', '|', '\n', '&'])
        || command.contains("$(")
        || command.contains('`')
        || command.contains("<<")
    {
        return None;
    }
    let args = astra_turn_core::evaluation::split_static_shell_words(command)?;
    let head = args.first()?.to_ascii_lowercase();
    if !crate::bash_intent::bash_command_looks_mutating(command) {
        return None;
    }
    // For redirects, only the destination is a mutation target; source paths
    // and string literals must not turn a scratch-only write into workspace
    // progress.  Handle both spaced and attached `>file` forms.
    let redirect_count = args
        .iter()
        .filter(|token| matches!(token.as_str(), ">" | ">>"))
        .count();
    if redirect_count > 0 {
        // A shell command may redirect more than one descriptor.  The static
        // lexer intentionally does not model descriptor semantics, so a
        // second redirect is an unknown additional write rather than a reason
        // to trust only the first target.
        if redirect_count != 1 {
            return None;
        }
        let index = args.iter().position(|token| token.starts_with('>'))?;
        if !matches!(head.as_str(), "cat" | "echo" | "printf") {
            return None;
        }
        let token = args[index].trim_start_matches('>');
        let target = if token.is_empty() {
            args.get(index + 1)?.to_string()
        } else {
            token.to_string()
        };
        return static_shell_path_token(&target).map(|target| vec![target]);
    }
    let positional = args
        .iter()
        .skip(1)
        .filter(|token| !token.starts_with('-'))
        .filter(|token| !token.contains('='))
        .cloned()
        .collect::<Vec<_>>();
    match head.as_str() {
        "cp" | "mv" | "ln" => {
            // Destination-changing options (notably `-t DIR`) need their own
            // argv semantics.  Until a typed executor target is available,
            // fail closed instead of treating the final source operand as the
            // destination.
            if args.iter().skip(1).any(|token| token.starts_with('-')) {
                return None;
            }
            positional
                .into_iter()
                .map(|target| static_shell_path_token(&target))
                .collect::<Option<Vec<_>>>()
                .and_then(|mut targets| targets.pop().map(|target| vec![target]))
        }
        "rm" | "rmdir" | "mkdir" | "touch" => {
            if args.iter().skip(1).any(|token| token.starts_with('-')) {
                return None;
            }
            positional
                .into_iter()
                .map(|target| static_shell_path_token(&target))
                .collect::<Option<Vec<_>>>()
                .filter(|targets| !targets.is_empty())
        }
        "chmod" | "chown" => {
            if args.iter().skip(1).any(|token| token.starts_with('-')) {
                return None;
            }
            // The first positional is a mode/owner, not a file.  Until the
            // executor provides typed target operands, retain only the final
            // literal path; counting the mode as a second workspace target
            // would let a one-file change buy the multi-file probation slice.
            positional
                .into_iter()
                .last()
                .and_then(|target| static_shell_path_token(&target))
                .map(|target| vec![target])
        }
        "sed" if args.iter().any(|token| token.starts_with("-i")) => {
            if args
                .iter()
                .skip(1)
                .any(|token| token.starts_with('-') && !token.starts_with("-i"))
            {
                return None;
            }
            positional
                .into_iter()
                .last()
                .and_then(|target| static_shell_path_token(&target))
                .map(|target| vec![target])
        }
        _ => None,
    }
}

/// Keep only literal path operands.  Expansion is deliberately not inferred
/// from a shell string: a variable, glob, tilde, escape, or substitution can
/// point outside the bound workspace and therefore needs a typed executor
/// receipt instead of scheduler heuristics.
fn static_shell_path_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty()
        || token.starts_with('&')
        || token.chars().any(|ch| {
            matches!(
                ch,
                '$' | '`' | '~' | '*' | '?' | '[' | ']' | '\\' | '>' | '<' | '|' | '&'
            )
        })
    {
        return None;
    }
    Some(token.to_string())
}

/// Prove a deliberately narrow shell shape: one literal redirect writer whose
/// sole target is volatile scratch outside the bound workspace. Compound
/// commands and multi-operand writers are not exhaustively modeled here; an
/// executor-provided typed receipt is required before those can be ignored by
/// a post-mutation observation watermark.
pub(crate) fn bash_mutation_is_proven_external_scratch(
    command: &str,
    workspace_root: Option<&str>,
) -> bool {
    // A review/exploration command often uses a compound read pipeline and
    // redirects the inspected bytes to `/tmp` for a second bounded read.  The
    // old recognizer rejected every compound form, so an external scratch
    // file was incorrectly promoted to a bound-workspace mutation.  Keep the
    // proof fail-closed: every segment must be either a canonical read-only
    // command (or shell-local `cd`) or a canonical read-only producer whose
    // sole redirect target is external scratch; unknown writers and any
    // workspace redirect remain mutation barriers.
    if let Some(segments) = astra_turn_core::evaluation::split_shell_control_segments(command) {
        let segments = segments
            .into_iter()
            .map(|segment| segment.trim().to_string())
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if external_scratch_for_loop(&segments, workspace_root) {
            return true;
        }
        if segments.len() > 1 {
            let mut saw_external_redirect = false;
            let mut safe = true;
            for segment in &segments {
                if let Some((producer, _target)) =
                    external_scratch_redirect(segment, workspace_root)
                {
                    if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(&producer)
                    {
                        safe = false;
                        break;
                    }
                    saw_external_redirect = true;
                } else if !shell_local_or_read_only(segment)
                    && !external_scratch_setup(segment, workspace_root)
                {
                    safe = false;
                    break;
                }
            }
            if safe && saw_external_redirect {
                return true;
            }
        }
    }

    let Some(segments) = astra_turn_core::evaluation::split_shell_control_segments(command) else {
        return false;
    };
    let mut segments = segments
        .into_iter()
        .map(|segment| segment.trim().to_string())
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 1 {
        return false;
    }
    let segment = segments.pop().expect("one non-empty segment");
    let head = segment
        .split_whitespace()
        .next()
        .map(|word| word.trim_matches(['\'', '"']).to_ascii_lowercase());
    if !matches!(head.as_deref(), Some("cat" | "echo" | "printf")) {
        return false;
    }
    let Some(targets) = simple_bash_mutation_targets(&segment) else {
        return false;
    };
    targets.len() == 1
        && Path::new(&targets[0]).is_absolute()
        && path_is_external_volatile_scratch(&targets[0], workspace_root)
}

/// Return the read-only producer and its one literal redirect target.  The
/// shell splitter has already separated pipelines/control operators; the
/// static lexer still rejects substitutions/globs so target scope cannot be
/// guessed from a shell string.
fn external_scratch_redirect(
    segment: &str,
    workspace_root: Option<&str>,
) -> Option<(String, String)> {
    // Normalize only AST-confirmed fd forwarding before looking for the one
    // real output target.  Without this, a safe `> /tmp/snapshot 2>&1` is
    // mistaken for two writes and cannot be scoped to external scratch.
    let normalized = astra_turn_core::cloud_approval_policy::strip_benign_fd_redirects(segment);
    let args = astra_turn_core::evaluation::split_static_shell_words(&normalized)?;
    let redirect_indices = args
        .iter()
        .enumerate()
        .filter(|(_, token)| token.starts_with('>') || token.starts_with("2>"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if redirect_indices.len() != 1 {
        return None;
    }
    let index = redirect_indices[0];
    let token = args[index].as_str();
    let target = if token == ">" || token == ">>" || token == "2>" || token == "2>>" {
        args.get(index + 1)?.clone()
    } else if token.starts_with("2>>") {
        token.trim_start_matches("2>>").to_string()
    } else {
        token
            .trim_start_matches(">>")
            .trim_start_matches('>')
            .to_string()
    };
    if target.is_empty()
        || target.contains(['$', '`', '~', '*', '?', '[', ']', '\\', '<', '>', '|', '&'])
        || !Path::new(&target).is_absolute()
        || !path_is_external_volatile_scratch(&target, workspace_root)
    {
        return None;
    }
    let mut producer = args[..index].to_vec();
    if token == ">" || token == ">>" || token == "2>" || token == "2>>" {
        // The target is a separate argv token; no producer bytes follow it.
        producer.truncate(index);
    }
    if producer.is_empty() {
        return None;
    }
    Some((producer.join(" "), target))
}

fn shell_local_or_read_only(segment: &str) -> bool {
    let head = segment
        .split_whitespace()
        .next()
        .map(|word| word.trim_matches(['\'', '"']).to_ascii_lowercase());
    head.as_deref() == Some("cd")
        || astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment)
}

/// Prove a small class of shell-local setup mutations whose every destination
/// is already outside the bound workspace.  Reviewers commonly create a
/// scratch directory before redirecting `git show` output into it; treating
/// `mkdir -p /tmp/...` as an opaque writer recreates the same false mutation
/// epoch as an external redirect.  Unknown writers, relative paths, and any
/// workspace target remain barriers.
fn external_scratch_setup(segment: &str, workspace_root: Option<&str>) -> bool {
    let normalized = astra_turn_core::cloud_approval_policy::strip_benign_fd_redirects(segment);
    let Some(args) = astra_turn_core::evaluation::split_static_shell_words(&normalized) else {
        return false;
    };
    let Some(head) = args.first().map(String::as_str) else {
        return false;
    };
    if !matches!(head, "mkdir" | "touch" | "rm") {
        return false;
    }
    let paths = args
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect::<Vec<_>>();
    !paths.is_empty()
        && paths.iter().all(|path| {
            Path::new(path).is_absolute() && path_is_external_volatile_scratch(path, workspace_root)
        })
}

/// Validate the bounded `for ...; do read-only body; done > /tmp/file` shape
/// used to collect several historical diff slices.  The shell evaluator
/// intentionally exposes only top-level segments, so validate the loop
/// header, substitute its literal loop variable in the body, and require
/// every body pipeline stage to be a canonical observation.  This handles a
/// real compound shell construct without treating arbitrary loops as safe.
fn external_scratch_for_loop(segments: &[String], workspace_root: Option<&str>) -> bool {
    let mut index = 0usize;
    let mut saw_loop = false;
    while index < segments.len() {
        let segment = segments[index].trim();
        if !segment.starts_with("for ") {
            if !shell_local_or_read_only(segment)
                && !external_scratch_setup(segment, workspace_root)
            {
                return false;
            }
            index += 1;
            continue;
        }
        if saw_loop || !valid_literal_for_header(segment) {
            return false;
        }
        saw_loop = true;
        let variable = segment
            .split_whitespace()
            .nth(1)
            .expect("validated for header has a loop variable");
        index += 1;
        let mut saw_body = false;
        let mut saw_done = false;
        while index < segments.len() {
            let body_segment = segments[index].trim();
            if let Some(body) = body_segment.strip_prefix("do ") {
                saw_body = true;
                if !read_only_loop_body(body, variable) {
                    return false;
                }
            } else if let Some(done_tail) = body_segment.strip_prefix("done") {
                if !saw_body || !external_scratch_done_redirect(done_tail, workspace_root) {
                    return false;
                }
                saw_done = true;
                index += 1;
                break;
            } else if !read_only_loop_body(body_segment, variable) {
                return false;
            }
            index += 1;
        }
        if !saw_done {
            return false;
        }
    }
    saw_loop
}

fn valid_literal_for_header(segment: &str) -> bool {
    let words = segment.split_whitespace().collect::<Vec<_>>();
    words.len() >= 4
        && words[0] == "for"
        && words[2] == "in"
        && words[1].bytes().enumerate().all(|(index, byte)| {
            (index == 0 && (byte == b'_' || byte.is_ascii_alphabetic()))
                || (index > 0 && (byte == b'_' || byte.is_ascii_alphanumeric()))
        })
        && words[3..].iter().all(|word| {
            !word.is_empty()
                && !word.contains(['$', '`', '*', '?', '[', ']', ';', '|', '&', '>', '<'])
        })
}

fn read_only_loop_body(body: &str, variable: &str) -> bool {
    let body = body
        .replace(&format!("${{{variable}}}"), "literal")
        .replace(&format!("${variable}"), "literal");
    let Some(pipeline_segments) = astra_turn_core::evaluation::split_shell_pipeline_segments(&body)
    else {
        return false;
    };
    !pipeline_segments.is_empty()
        && pipeline_segments
            .into_iter()
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .all(shell_local_or_read_only)
}

fn external_scratch_done_redirect(done_tail: &str, workspace_root: Option<&str>) -> bool {
    // Reuse the exact redirect parser with a harmless read-only producer;
    // `done` itself is a shell keyword, not an argv command.
    let synthetic = format!("echo{done_tail}");
    external_scratch_redirect(&synthetic, workspace_root).is_some()
}

fn record_is_stable_workspace_mutation(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    // A structured writer is stable only after its owner has reported that
    // the commit actually happened in the bound workspace.  Tool name,
    // args, exit=0, or a dry-run shape are admission/risk facts, not budget
    // evidence.
    if record_has_typed_workspace_tool_receipt(record) {
        return record.ok;
    }
    if record.name == "bash"
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
        && record
            .workspace_mutation_receipt
            .as_ref()
            .is_some_and(astra_tools::workspace_observation::is_authoritative_changed_receipt)
    {
        return record.ok;
    }
    let _ = state;
    // Unknown/malformed/direct-name mutations are not evidence of a stable
    // deliverable. The admission classifier may still execute them, but the
    // scheduler must not renew on an unscoped or dry-run mutation.
    false
}

fn stable_workspace_targets(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> Vec<PathBuf> {
    if !record_is_stable_workspace_mutation(state, record) {
        return Vec::new();
    }
    let root = state.hooks.workspace_root_hint.as_deref();
    if let Some(path) =
        record_explicit_path(record).and_then(|path| normalize_workspace_path(&path, root))
    {
        return vec![path];
    }
    if record.name != "bash" {
        return Vec::new();
    }
    let Some(command) = extract_tool_args(record.authoritative_args_full()).and_then(|args| {
        astra_turn_core::tool_argument_hints::command_hint_from_args(&args).map(str::to_string)
    }) else {
        return Vec::new();
    };
    let Some(segments) = astra_turn_core::evaluation::split_shell_control_segments(&command) else {
        return Vec::new();
    };
    segments
        .into_iter()
        .filter_map(|segment| simple_bash_mutation_targets(segment.trim()))
        .flatten()
        .filter_map(|target| normalize_workspace_path(&target, root))
        .collect()
}

fn record_is_successful_workspace_validation(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
    latest_mutation_path: Option<&str>,
) -> bool {
    if !record.was_executed()
        || !record.ok
        || matches!(
            record.result_class.as_deref(),
            Some(
                "test_failure"
                    | "env_failure"
                    | "execution_error"
                    | "inconclusive"
                    | "agent_incomplete"
                    | "fanout_incomplete"
            )
        )
    {
        return false;
    }
    if record.name != "bash" {
        return false;
    }
    let Some(args) = extract_tool_args(record.authoritative_args_full()) else {
        return false;
    };
    let Some(command) = astra_turn_core::tool_argument_hints::command_hint_from_args(&args) else {
        return false;
    };
    if !bash_command_is_workspace_scoped(command, state.hooks.workspace_root_hint.as_deref()) {
        return false;
    }
    // A mutating compound command is a receipt only when a positive
    // validation occurs after its final mutation.  Do not let a successful
    // `cargo test && sed -i ...` masquerade as validation of the modified
    // state merely because its prefix matches a known validator.
    // This ordering check applies to every bash validation record.  The
    // side-effect classifier is intentionally conservative and can miss an
    // unknown writer (for example an inline Python mutation); a recognized
    // validation prefix must not bypass the post-mutation barrier in that
    // case.
    if !bash_command_has_proportionate_validation(command) {
        return false;
    }
    if astra_turn_core::evaluation::bash_command_post_mutation_validation_prefix(command).is_some()
    {
        return true;
    }
    // Artifact-local checks are only completion evidence when they name the
    // latest mutation target.  Without a typed target, keep the result as
    // ordinary observation rather than granting another scheduler slice.
    let Some(path) = latest_mutation_path else {
        return false;
    };
    let Some(receipt_operands) = local_validation_receipt_operands(command) else {
        return false;
    };
    let target = normalize_absolute_path(path);
    receipt_operands.iter().any(|token| {
        token == path
            || target.as_ref().is_some_and(|target| {
                normalize_absolute_path(token).is_some_and(|candidate| candidate == *target)
            })
    })
}

fn bash_command_has_proportionate_validation(command: &str) -> bool {
    if astra_turn_core::evaluation::bash_command_post_mutation_validation_prefix(command).is_some()
    {
        return true;
    }
    local_validation_receipt_operands(command).is_some()
}

/// Return the operands of the final artifact-local receipt in a command.
/// Keeping the operands attached to the receipt segment is important: a
/// mutation segment necessarily contains its own target, but that does not
/// mean a later `cmp`/checksum actually validated it.  Control operators are
/// interpreted conservatively so `;`/`||` cannot preserve a receipt after an
/// alternate status path.
fn local_validation_receipt_operands(command: &str) -> Option<Vec<String>> {
    let segments = astra_turn_core::evaluation::split_shell_control_segments_with_ops(command)?;
    let mut receipt_operands = None;
    for (segment_index, (raw_segment, op_after)) in segments.iter().enumerate() {
        let segment = raw_segment.trim();
        if !segment.is_empty() {
            if let Some(operands) = local_validation_segment_operands(segment) {
                receipt_operands = Some(operands);
            } else if !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(segment)
                && !local_status_neutral_segment(segment)
            {
                receipt_operands = None;
            }
        }
        let sequence_has_rhs = segments[segment_index + 1..].iter().any(|(segment, _)| {
            let segment = segment.trim();
            !segment.is_empty() && !segment.starts_with('#')
        });
        if matches!(op_after, astra_turn_core::evaluation::ShellControlOp::Or)
            || (matches!(
                op_after,
                astra_turn_core::evaluation::ShellControlOp::Sequence
            ) && sequence_has_rhs)
        {
            receipt_operands = None;
        }
    }
    receipt_operands
}

fn local_status_neutral_segment(segment: &str) -> bool {
    matches!(segment.trim().to_ascii_lowercase().as_str(), "true" | ":")
}

fn local_validation_segment_operands(segment: &str) -> Option<Vec<String>> {
    if segment.contains('|') {
        return None;
    }
    let words = astra_turn_core::evaluation::split_static_shell_words(segment)?;
    let command = words.first().map(|word| word.to_ascii_lowercase())?;
    let operands = words.iter().skip(1);
    match command.as_str() {
        // `file` and a raw checksum are observations, not proof that the
        // artifact satisfies a requested contract.  `-c` makes a checksum
        // command compare against an expected manifest.
        "sha256sum" | "sha512sum" if operands.clone().any(|word| word == "-c") => Some(
            operands
                .filter(|word| !word.starts_with('-'))
                .cloned()
                .collect(),
        ),
        // Comparing an artifact with itself is another observation; require
        // two distinct operands before it can be scoped to the latest target.
        "cmp" | "diff" => {
            let operands = operands
                .filter(|word| !word.starts_with('-'))
                .collect::<Vec<_>>();
            (operands.len() >= 2 && operands.windows(2).any(|pair| pair[0] != pair[1]))
                .then(|| operands.into_iter().cloned().collect())
        }
        // A bare `test -e/-s` is an observation of existence/metadata, not a
        // correctness receipt without an expected value or typed contract.
        "[" => None,
        _ => None,
    }
}

fn recent_window_has_stable_mutation(state: &AgenticLoopState, recent_start: usize) -> bool {
    state.stall.tool_call_records[recent_start..]
        .iter()
        .any(|record| record_is_stable_workspace_mutation(state, record))
}

fn recent_window_has_validated_stable_mutation(
    state: &AgenticLoopState,
    recent_start: usize,
) -> bool {
    let mut saw_stable_mutation = false;
    let mut latest_mutation_is_validated = false;
    let mut latest_mutation_path: Option<String> = None;
    for record in state.stall.tool_call_records[recent_start..]
        .iter()
        .filter(|record| record.was_executed())
    {
        let args = extract_tool_args(record.authoritative_args_full());
        let stable_mutation = record_is_stable_workspace_mutation(state, record);
        if stable_mutation && record.ok {
            saw_stable_mutation = true;
            latest_mutation_is_validated = false;
            latest_mutation_path = stable_workspace_targets(state, record)
                .into_iter()
                .last()
                .and_then(|path| path.to_str().map(ToString::to_string));
        }
        let successful_validation = record_is_successful_workspace_validation(
            state,
            record,
            latest_mutation_path.as_deref(),
        );
        if !stable_mutation
            && crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                &record.name,
                args.as_ref(),
            )
            && !successful_validation
        {
            // An unscoped or volatile mutation invalidates the previous
            // validation epoch just as surely as a stable mutation does.  This
            // includes a command that wrote bytes before returning a failure;
            // an exit status is not a rollback receipt.
            latest_mutation_is_validated = false;
            latest_mutation_path = None;
        }
        if saw_stable_mutation && successful_validation {
            latest_mutation_is_validated = true;
        }
    }
    saw_stable_mutation && latest_mutation_is_validated
}

/// Permit one bounded probationary slice for a normal multi-file write plan.
///
/// A task often has to create several deliverables before any meaningful
/// project-wide check can run.  Requiring validation after the very first
/// write would make the scheduler settle before the plan is even materialized.
/// This escape hatch is intentionally one-shot and narrow: it only accepts a
/// wholly successful window of distinct, explicitly scoped workspace writes.
/// Subsequent slices still require validation, acceptance, or typed recovery.
fn recent_window_supports_probationary_extension(
    state: &AgenticLoopState,
    recent_start: usize,
) -> bool {
    if used_budget_extensions(state) != 0 {
        return false;
    }
    let recent = &state.stall.tool_call_records[recent_start..];
    if recent.len() < 2
        || recent.iter().any(|record| {
            !record.was_executed() || !record.ok || record_failure_or_rejection(record)
        })
    {
        return false;
    }
    let mut targets = HashSet::new();
    let mut saw_validation_attempt = false;
    for record in recent {
        if record_is_stable_workspace_mutation(state, record) {
            for path in stable_workspace_targets(state, record) {
                targets.insert(path);
            }
            continue;
        }
        // Unknown or volatile mutations are a hard barrier.  Successful
        // read-only observations are allowed in the probationary window, but
        // arbitrary opaque calls are not evidence of a deliverable.
        if tool_record_is_workspace_mutation(record)
            || !crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                &record.name,
                extract_tool_args(record.authoritative_args_full()).as_ref(),
            )
        {
            return false;
        }
        if record.name == "bash"
            && extract_tool_args(record.authoritative_args_full())
                .and_then(|args| {
                    astra_turn_core::tool_argument_hints::command_hint_from_args(&args)
                        .map(bash_command_has_proportionate_validation)
                })
                .unwrap_or(false)
        {
            saw_validation_attempt = true;
        }
    }
    if saw_validation_attempt {
        // Once a validation-shaped command appears in the window, a later
        // mutation must close the epoch; do not let the one-shot probation
        // path bypass the ordering check.
        return false;
    }
    if targets.len() < 2 {
        return false;
    }
    let mut signatures = recent
        .iter()
        .filter_map(|record| record.round.map(|_| record.name.as_str()))
        .collect::<HashSet<_>>();
    // Round signatures are checked by the generic repetition guard.  Keep a
    // second, cheap distinctness check here for synthetic records that do not
    // carry round ids, without comparing task- or provider-specific text.
    signatures.len() >= 2 || {
        signatures.clear();
        recent
            .iter()
            .filter_map(|record| record.authoritative_args_full())
            .for_each(|args| {
                signatures.insert(args);
            });
        signatures.len() >= 2
    }
}

fn recent_window_completed_explicit_acceptance(
    state: &AgenticLoopState,
    recent_start: usize,
) -> bool {
    if !recent_window_has_stable_mutation(state, recent_start)
        || !super::execution_phase::missing_explicit_verification_hooks(state)
            .is_some_and(|missing| missing.is_empty())
    {
        return false;
    }
    let mut acceptance_seen = false;
    for record in &state.stall.tool_call_records[recent_start..] {
        if acceptance_seen {
            let args = extract_tool_args(record.authoritative_args_full());
            if crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                &record.name,
                args.as_ref(),
            ) {
                return false;
            }
        }
        if record.was_executed()
            && record.ok
            && state.hooks.stop_hooks.iter().any(|hook| {
                hook.authoritative
                    && super::execution_phase::record_verifies_explicit_hook(record, hook)
            })
        {
            acceptance_seen = true;
        }
    }
    acceptance_seen
}

/// Return whether the recent execution window contains a typed evidence
/// delta, rather than merely a different command/range spelling.  Budget
/// renewal is advisory capacity management, so a healthy long investigation
/// can continue when it produces new receipts, but a sequence of equivalent
/// reads must settle instead of buying more slices forever.
fn recent_window_has_typed_evidence_delta(
    state: &AgenticLoopState,
    recent_start: usize,
    allow_workspace_observation_delta: bool,
) -> bool {
    let records = &state.stall.tool_call_records;
    if recent_start >= records.len() {
        return false;
    }
    let prior = &records[..recent_start];
    let recent = &records[recent_start..];
    let consumed_extension_count = used_budget_extensions(state);
    let current_slice_floor = if consumed_extension_count > 0 {
        state
            .max_turns
            .saturating_sub(state.agentic_turn_budget.extension_turns)
    } else {
        0
    };

    let mut seen_locations: HashSet<String> =
        prior.iter().filter_map(record_explicit_path).collect();
    // Search/diff/git observations often carry their scope in a structured
    // argument rather than the direct `path` field.  Keep the same canonical
    // operation identity used by terminal evaluation so a new query is
    // evidence without treating a repeated query as progress.  Requiring a
    // prior observation (or two distinct observations in this window) mirrors
    // the path rule below and prevents one isolated read from minting a slice.
    let mut seen_observation_operations = HashSet::new();
    for record in prior.iter().filter(|record| {
        record.was_executed()
            && (current_slice_floor == 0
                || record
                    .round
                    .is_some_and(|round| (round as usize) >= current_slice_floor))
    }) {
        if record_can_observe_bound_workspace(state, record)
            && !tool_record_is_workspace_mutation(record)
            && let Some(key) = astra_turn_core::evaluation::tool_outcome_operation_key(record)
        {
            seen_observation_operations.insert(key);
        }
    }
    // A successful retry of the exact operation that previously failed or
    // was rejected is a typed recovery delta.  Do not infer equivalence from
    // tool names alone; the core evaluator owns the canonical operation key.
    // Reconstruct the unresolved ledger at the boundary, rather than keeping
    // every historical failure forever.  A recovery is a state transition
    // (unresolved -> resolved), not a property of the operation name that can
    // be replayed by identical successes in later slices.
    let mut unresolved_operations = HashSet::new();
    for record in prior.iter().filter(|record| record.was_executed()) {
        if let Some(key) = astra_turn_core::evaluation::tool_outcome_operation_key(record) {
            if record_failure_or_rejection(record) {
                unresolved_operations.insert(key);
            } else if record.ok {
                unresolved_operations.remove(&key);
            }
        }
    }

    // Compare each new record with the evidence already observed, including
    // earlier records in this same recent window. This keeps a two-call
    // boundary useful when it is the first window, while still rejecting one
    // isolated read as sufficient progress.
    for record in recent.iter().filter(|record| record.was_executed()) {
        let observation_is_successful = record.ok
            && record_can_observe_bound_workspace(state, record)
            && !tool_record_is_workspace_mutation(record);
        if allow_workspace_observation_delta
            && observation_is_successful
            && let Some(key) = astra_turn_core::evaluation::tool_outcome_operation_key(record)
            && !seen_observation_operations.is_empty()
            && seen_observation_operations.insert(key)
        {
            return true;
        }
        let args = extract_tool_args(record.authoritative_args_full());
        if allow_workspace_observation_delta
            && record.ok
            && !tool_record_is_workspace_mutation(record)
            && crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                &record.name,
                args.as_ref(),
            )
            && record_explicit_path(record)
                .is_some_and(|path| !seen_locations.is_empty() && !seen_locations.contains(&path))
        {
            return true;
        }
        if let Some(path) = record_explicit_path(record) {
            seen_locations.insert(path);
        }

        if record_failure_or_rejection(record) {
            if let Some(key) = astra_turn_core::evaluation::tool_outcome_operation_key(record) {
                unresolved_operations.insert(key);
            }
        } else if record.ok
            && astra_turn_core::evaluation::tool_outcome_operation_key(record)
                .is_some_and(|key| unresolved_operations.remove(&key))
            && (!tool_record_is_workspace_mutation(record)
                || record_is_stable_workspace_mutation(state, record))
        {
            return true;
        }
    }
    false
}

/// Unknown/opaque writers only close the evidence window in which they ran.
/// A later read-only window can return to the ordinary observation policy if
/// no concrete mutation shape was ever recorded. This keeps admission safety
/// fail-closed without reclassifying a whole read-only task forever.
fn recent_window_has_executed_mutation_risk(state: &AgenticLoopState, recent_start: usize) -> bool {
    state.stall.tool_call_records[recent_start..]
        .iter()
        .filter(|record| record.was_executed())
        .any(|record| {
            let args = extract_tool_args(record.authoritative_args_full());
            crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                &record.name,
                args.as_ref(),
            ) && !record_is_successful_workspace_validation(state, record, None)
        })
}

/// Decide whether the next bounded slice has a fresh, task-facing reason to
/// exist. Read-only work may advance through distinct workspace observations;
/// mutating work must instead close a stable mutation epoch with a successful
/// validator or an explicit acceptance contract. Exact typed recovery remains
/// valid for both. Tool volume, a new result-class label, and scratch writes are
/// deliberately not progress proofs.
fn recent_activity_supports_budget_extension(state: &AgenticLoopState) -> bool {
    if super::execution_phase::workspace_observation_is_quarantined(state) {
        // A quarantined workspace has no trustworthy evidence watermark. Do
        // not renew a slice from ledger-shaped reads or validation records;
        // the turn must end as unverified until a new bound is established.
        return false;
    }
    const RECENT_ACTIVITY_WINDOW: usize = 8;
    let consumed_extension_count = used_budget_extensions(state);
    let current_slice_floor = if consumed_extension_count > 0 {
        state
            .max_turns
            .saturating_sub(state.agentic_turn_budget.extension_turns)
    } else {
        0
    };

    let recent_indices: Vec<usize> = state
        .stall
        .tool_call_records
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, record)| {
            record.was_executed()
                && (current_slice_floor == 0
                    || record
                        .round
                        .is_some_and(|round| (round as usize) >= current_slice_floor))
        })
        .take(RECENT_ACTIVITY_WINDOW)
        .map(|(index, _)| index)
        .collect();
    let Some(recent_start) = recent_indices.last().copied() else {
        return false;
    };
    if recent_turns_are_repetitive(state) {
        return false;
    }

    let effective_mutating_evidence_mode = state.task_profile.mutates_workspace
        || super::execution_phase::has_executed_positive_workspace_mutation(state)
        || recent_window_has_executed_mutation_risk(state, recent_start);

    recent_window_has_typed_evidence_delta(state, recent_start, !effective_mutating_evidence_mode)
        || recent_window_has_validated_stable_mutation(state, recent_start)
        || recent_window_completed_explicit_acceptance(state, recent_start)
        || recent_window_supports_probationary_extension(state, recent_start)
}

fn maybe_extend_turn_budget(state: &mut AgenticLoopState) -> Option<()> {
    // Runtime-policy entries are alerts, not a second scheduler.  A
    // convergence-stage advisory can still accompany a productive, bounded
    // slice (for example, a complex investigation with changing evidence).
    // Concrete progress, explicit verdicts, repeated signatures, and the
    // administrator-owned hard limit remain the execution controls.
    let budget = state.agentic_turn_budget;
    let at_review_limit = state.max_turns >= budget.hard_turn_limit;
    if state.hooks.completion_settlement.text_only
        || state.hooks.completion_settlement.work_settlement_only
        || budget.extension_turns == 0
        || budget.max_extensions == 0
        || (at_review_limit && !budget.renewable_past_review_limit)
        || (!at_review_limit && used_budget_extensions(state) >= budget.max_extensions)
        || crate::server::run::lifecycle::has_turn_verdict_critical(&state.stall.verdict_events)
        || !recent_activity_supports_budget_extension(state)
    {
        return None;
    }

    // Profile limits are adaptive review checkpoints, not semantic task
    // boundaries. A renewable profile keeps receiving bounded slices while
    // concrete progress continues. Explicit caller/child limits retain a
    // non-renewable hard boundary after any configured headroom is consumed.
    let available = if at_review_limit {
        usize::MAX.saturating_sub(state.max_turns)
    } else {
        budget.hard_turn_limit.saturating_sub(state.max_turns)
    };
    let additional_turns = budget.extension_turns.min(available);
    if additional_turns == 0 {
        return None;
    }

    let previous_max_turns = state.max_turns;
    let previous_remaining_turns = state.remaining_turns;
    state.max_turns += additional_turns;
    state.remaining_turns += additional_turns;
    tracing::info!(
        target: "astra::budget",
        previous_max_turns,
        previous_remaining_turns,
        max_turns = state.max_turns,
        remaining_turns = state.remaining_turns,
        additional_turns,
        profile_review_limit = budget.hard_turn_limit,
        "runtime renewed adaptive agentic execution slice"
    );
    Some(())
}

/// Shared accounting primitive for a one-time fallback initial-slice
/// correction. Semantic/execution evidence is checked by the two callers.
/// This never changes the already-resolved hard ceiling, extension policy, or
/// explicit caller limit.
fn promote_untouched_fallback_initial_slice(state: &mut AgenticLoopState) -> bool {
    let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
    if state.budget_is_explicit
        || state.agentic_turn_budget.initial_turns != fallback.agentic_turn_budget.initial_turns
        || state.max_turns != state.agentic_turn_budget.initial_turns
    {
        return false;
    }

    let implementation =
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            false,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
        );
    let promoted_initial = implementation
        .agentic_turn_budget
        .initial_turns
        .min(state.agentic_turn_budget.hard_turn_limit);
    let additional_turns = promoted_initial.saturating_sub(state.max_turns);
    // The runtime ceiling and renewal contract were already resolved at the
    // request boundary. Preserve them exactly; only restore the missing
    // implementation portion of the initial slice.
    state.agentic_turn_budget.initial_turns = promoted_initial;
    state.max_turns = promoted_initial;
    state.remaining_turns = state.remaining_turns.saturating_add(additional_turns);
    tracing::info!(
        target: "astra::budget",
        additional_turns,
        initial_turns = state.max_turns,
        hard_turn_limit = state.agentic_turn_budget.hard_turn_limit,
        "promoted fallback analysis slice after trusted workspace mutation"
    );
    true
}

/// Reconcile an authoritative Work admission with the still-untouched
/// fallback analysis slice. This changes only the initial review checkpoint;
/// the caller-owned hard ceiling, extension size/count, and task profile remain
/// independent authorities.
pub(crate) fn promote_fallback_budget_for_authoritative_mutation(
    state: &mut AgenticLoopState,
) -> bool {
    let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
    if state.task_profile != fallback {
        return false;
    }
    promote_untouched_fallback_initial_slice(state)
}

fn promote_fallback_budget_after_observed_mutation(state: &mut AgenticLoopState) -> bool {
    // Primary turns intentionally begin without text-derived semantic
    // classification. A successful, stable bound-workspace mutation is an
    // executor-owned fact that can correct the initial review checkpoint, but
    // it still does not become user intent or authorize further mutation.
    let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
    if state.turn_intent.is_some()
        || state.task_profile != fallback
        || super::execution_phase::workspace_observation_is_quarantined(state)
        || !state.stall.tool_call_records.iter().any(|record| {
            record.was_executed() && record_is_stable_workspace_mutation(state, record)
        })
    {
        return false;
    }
    promote_untouched_fallback_initial_slice(state)
}

/// Reserve a provider boundary for a user-visible answer before the adaptive
/// agentic budget is consumed by another tool round.
///
/// A non-renewable limit or typed no-progress decision must not consume the
/// only boundary on which the model can synthesize its result. The extra
/// boundary is text-only unless a canonical Work attempt first needs its typed
/// settlement operation, so it cannot reopen exploratory execution. Explicitly
/// small caller budgets receive the same settlement guarantee after at least
/// one completed tool call.
pub(crate) fn adaptive_budget_is_renewable(state: &AgenticLoopState) -> bool {
    let budget = state.agentic_turn_budget;
    budget.extension_turns > 0
        && budget.max_extensions > 0
        && !crate::server::run::lifecycle::has_turn_verdict_critical(&state.stall.verdict_events)
        && if state.max_turns >= budget.hard_turn_limit {
            budget.renewable_past_review_limit
        } else {
            used_budget_extensions(state) < budget.max_extensions
        }
}

fn begin_budget_settlement(state: &mut AgenticLoopState) -> bool {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    begin_budget_settlement_for_work_state(state, active_work_attempt)
}

fn begin_budget_settlement_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
) -> bool {
    if state.hooks.completion_settlement.text_only
        || state.hooks.completion_settlement.work_settlement_only
        || state
            .hooks
            .completion_settlement
            .completion_action_window
            .is_some()
        || state.budget_wrapup_injected
        || state.interruption.is_some()
        || completed_tool_calls(state) == 0
    {
        return false;
    }

    if let Some(action) = super::execution_phase::pending_terminal_completion_action_for_work_state(
        state,
        active_work_attempt,
    ) && !super::execution_phase::completion_action_window_is_batchable(state, &action)
    {
        // Do not advertise a one-call window for a dependency chain that
        // cannot be completed atomically. The terminal branch records a
        // truthful incomplete result instead of inviting a misleading retry.
        return false;
    }
    state.hooks.completion_settlement.work_settlement_only = active_work_attempt;
    state.hooks.completion_settlement.wrapup_origin = Some(BudgetWrapupOrigin::RoundSlice);

    if let Some(action) = super::execution_phase::pending_terminal_completion_action_for_work_state(
        state,
        active_work_attempt,
    ) && super::execution_phase::completion_action_window_is_batchable(state, &action)
        && !matches!(action, super::host::CompletionAction::CompletionTaskAction)
    {
        // Reserve exactly one matching completion action and one closing
        // boundary. For an active Work attempt the second boundary is the
        // canonical settle_work_item operation; otherwise it is final text.
        // This is a typed settlement allowance, not ordinary exploration and
        // not a change to the hard review ceiling.
        state.max_turns = state.max_turns.saturating_add(2);
        state.remaining_turns = state.remaining_turns.saturating_add(2);
        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.completion_action_window = Some(CompletionActionWindow {
            action: action.clone(),
            attempts_remaining: 1,
            mismatch_corrections_remaining: 1,
            consumed: false,
            matched: false,
        });
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "typed_completion_action_available",
                "mode": if active_work_attempt { "completion_then_work_settlement" } else { "one_completion_action" },
                "allowed_action": action,
                "attempts_remaining": 1,
                "action_hint": super::execution_phase::completion_action_hint(&action),
                "declarations_may_remain_visible_for_cache": true,
                "execution_authority": "one_matching_action",
                "instruction": if active_work_attempt {
                    "Perform exactly one action matching the declared completion obligation, then settle the currently owned WorkItem truthfully with settle_work_item. Do not resume ordinary exploration or request an unrelated tool."
                } else {
                    "Perform exactly one action matching the declared completion obligation, then produce the final answer. Do not resume ordinary exploration or request an unrelated tool."
                },
                "authority": "typed_turn_intent_and_executed_tool_ledger",
            }),
        );
        return true;
    }

    // Preserve the number of already-consumed rounds while adding one
    // non-agentic settlement boundary. This is accounting headroom, not a
    // renewed tool budget: text_only is enforced by both schema projection and
    // tool admission.
    state.max_turns = state.max_turns.saturating_add(1);
    state.remaining_turns = state.remaining_turns.saturating_add(1);
    state.hooks.completion_settlement.text_only = !active_work_attempt;
    state.budget_wrapup_injected = !active_work_attempt;
    let instruction = if active_work_attempt {
        "Exploration is no longer making sufficient progress. Settle the currently owned WorkItem now with its truthful typed outcome (delivered, blocked, or failed). Do not request any other tool, create tasks, or delegate work."
    } else {
        "The bounded tool-execution slice is complete. Answer the user now from the evidence already gathered. Do not narrate this runtime boundary, request tools, create tasks, delegate work, or promise a future action (for example, `I will run` or `let me check`). If requested work remains, state it as unfinished rather than describing it as about to happen."
    };
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "completion_settlement.v2",
            "signal": "agentic_execution_slice_complete",
            "mode": if active_work_attempt { "work_settlement_only" } else { "text_only" },
            "allowed_action": if active_work_attempt { serde_json::json!("settle_work_item") } else { serde_json::Value::Null },
            "attempts_remaining": if active_work_attempt { 1 } else { 0 },
            "declarations_may_remain_visible_for_cache": true,
            "execution_authority": if active_work_attempt { "one_matching_action" } else { "none" },
            "evidence": {
                "tool_calls_completed": completed_tool_calls(state),
                "rounds_completed": current_agentic_step(state),
            },
            "instruction": instruction,
            "authority": "runtime_bounded_settlement",
        }),
    );
    true
}

fn reserve_budget_settlement_boundary(state: &mut AgenticLoopState) {
    if state.remaining_turns == 1 && !adaptive_budget_is_renewable(state) {
        let _ = begin_budget_settlement(state);
    }
}

/// Unified diagnosis of all stall conditions evaluated at interruption time.
///
/// Computes the signal string, human-readable summary, and restricted tool
/// list in a single pass to avoid re-evaluating the same three stall
/// conditions (exploration family streak, redundant reads, single-tool
/// streak) across three separate functions.
struct StallDiagnosis {
    signal: Option<String>,
    summary: Option<String>,
    restricted_tools: Vec<String>,
}

fn compute_stall_diagnosis(state: &AgenticLoopState) -> StallDiagnosis {
    // 1. Exploration family streak (read/search/diff dominance)
    if let Some((family, streak)) =
        astra_turn_core::evaluation::exploration_family_round_streak(&state.stall.tool_call_records)
        && streak >= astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
    {
        let mut summary_text = match family {
            "read" => format!("{streak} consecutive read-dominant exploratory rounds"),
            "search" => format!("{streak} consecutive search-dominant exploratory rounds"),
            "diff" => format!("{streak} consecutive diff-dominant exploratory rounds"),
            _ => format!("{streak} consecutive {family}-dominant exploratory rounds"),
        };
        // Enrich with redundant read detail when co-occurring
        if family == "read" {
            let redundant_reads = astra_turn_core::evaluation::count_redundant_overlapping_reads(
                &state.stall.tool_call_records,
            );
            if redundant_reads >= astra_turn_core::evaluation::REDUNDANT_OVERLAPPING_READS_THRESHOLD
            {
                summary_text.push_str(&format!(
                    "; {redundant_reads} redundant overlapping reads on unchanged files"
                ));
            }
        }
        let summary = Some(summary_text);
        return StallDiagnosis {
            signal: Some(format!("exploration_family={family};streak={streak}")),
            summary,
            restricted_tools: Vec::new(),
        };
    }

    // 2. Redundant overlapping reads
    let redundant_reads = astra_turn_core::evaluation::count_redundant_overlapping_reads(
        &state.stall.tool_call_records,
    );
    if redundant_reads >= astra_turn_core::evaluation::REDUNDANT_OVERLAPPING_READS_THRESHOLD {
        return StallDiagnosis {
            signal: Some(format!("redundant_reads={redundant_reads}")),
            summary: Some(format!(
                "{redundant_reads} redundant overlapping reads on unchanged files"
            )),
            restricted_tools: Vec::new(),
        };
    }

    StallDiagnosis {
        signal: None,
        summary: None,
        restricted_tools: Vec::new(),
    }
}

/// Build an interruption state summary from the current loop state.
pub(crate) fn interruption_state_summary(
    state: &AgenticLoopState,
    error_detail: Option<String>,
) -> InterruptionStateSummary {
    let diag = compute_stall_diagnosis(state);
    InterruptionStateSummary {
        has_checkpoint: state.stall.last_heavy_checkpoint.is_some(),
        tool_calls_completed: completed_tool_calls(state),
        turns_completed: current_agentic_step(state),
        remaining_turns: state.remaining_turns as u32,
        error_detail,
        stall_signal: diag.signal,
        resume_restricted_tools: diag.restricted_tools,
    }
}

pub(crate) fn interruption_diagnosis_summary(state: &AgenticLoopState) -> Option<String> {
    compute_stall_diagnosis(state).summary
}

pub(crate) fn mark_work_settlement_incomplete(state: &mut AgenticLoopState) {
    state.final_text = super::host::WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT.to_string();
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some("canonical Work still had an unsettled item at the terminal boundary".into()),
        ),
    ));
}

pub(crate) async fn run_loop_preamble<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    // This flag is a turn outcome consumed by the canonical commit after the
    // agentic loop returns. Reset it when a new turn actually starts, not
    // during finalization, so prefix rewrites remain observable to commit.
    state.context_compression_triggered = false;

    if state
        .skills
        .session_event_hooks
        .has_event(crate::skills::hooks::SessionEvent::SessionStart)
    {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_msg = state.message.as_str();
        let hook_output = crate::skills::hooks::evaluate_session_hooks(
            &state.skills.session_event_hooks,
            crate::skills::hooks::SessionEvent::SessionStart,
            session_id,
            Some(user_msg),
        )
        .await;
        if let Some(ctx) = hook_output.context {
            state.push_volatile_payload(
                super::host::VolatileKind::SessionHookContext,
                serde_json::json!({
                    "event": "session_start",
                    "context": ctx,
                }),
            );
        }
        for (key, value) in hook_output.env_vars {
            astra_core::session_env_overlay::set(&key, &value);
        }
    }

    if let Some(resolver) = &state.skills.resolver {
        // Phase-9: `skill_tool_schema_v2` is a byte-stable constant — it
        // takes no skill list and advertises `skill_name` as an open
        // string. The catalog is surfaced via `<available_skills>` in the
        // session-cached prompt prefix (see `build_skill_listing_section`),
        // so adding/removing a skill no longer perturbs the tool schema
        // bytes.
        if !resolver.available_skills().is_empty() {
            host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema_v2());
        }
    }

    // NOTE: Cross-Session Project Context used to be injected into
    // `state.messages` here as a system message. It has moved into the
    // context pipeline's `ProjectContext` section (bound from
    // `SessionContext.project_context`) so it sits in `CacheScope::Session`
    // BEFORE the Session→None marker — now it participates in the cached
    // session prefix instead of being re-sent after the marker every turn.
    // See `context_pipeline_adapter::build_session_context` + `bind_project_context`.
}

fn apply_structured_user_reanchor(
    state: &mut AgenticLoopState,
    relation: astra_turn_types::ObjectiveRelation,
) -> bool {
    if state.message.trim().is_empty() {
        return false;
    }

    state.turn_guard.begin_fresh_user_turn();
    // Hard capability/permission restrictions are owned by their boundary and
    // must survive a semantic re-anchor. Behavioral state can reset here, but
    // user intent must not broaden the executable capability surface.
    state.boosted_tools.clear();
    state.widen_selection_pending = true;

    if let Some(session) = state.pipeline_session.as_mut() {
        match relation {
            astra_turn_types::ObjectiveRelation::Replace => session
                .working_memory_mut()
                .apply_objective_replacement(&state.message),
            astra_turn_types::ObjectiveRelation::Correct => session
                .working_memory_mut()
                .apply_user_correction(&state.message),
            astra_turn_types::ObjectiveRelation::Unknown
            | astra_turn_types::ObjectiveRelation::Acknowledge
            | astra_turn_types::ObjectiveRelation::Continue
            | astra_turn_types::ObjectiveRelation::Refine => {}
        }
    }
    true
}

fn apply_structured_user_feedback(state: &mut AgenticLoopState, intent: &TurnIntent) -> bool {
    let Some(feedback) = intent.feedback else {
        return false;
    };
    // The re-anchor path already records corrections while resetting stale
    // transient state. Avoid duplicating the same exact feedback entry.
    if intent.reanchors_current_objective()
        && feedback.kind == astra_turn_types::UserFeedbackKind::Correction
    {
        return false;
    }
    let Some(session) = state.pipeline_session.as_mut() else {
        return false;
    };
    session
        .working_memory_mut()
        .apply_user_feedback(feedback, &state.message);
    true
}

/// Estimate context pressure from raw message token count.
///
/// Returns `(pressure, estimated_tokens)` where pressure is a 0.0–1.0
/// ratio against `max_turn_input_tokens`. When no limit is configured
/// (`max_turn_input_tokens == 0`) returns `(0.0, 0)`.
#[inline]
pub(crate) fn estimate_context_pressure(
    messages: &[serde_json::Value],
    pinned_tool_schema_tokens: usize,
    max_turn_input_tokens: u64,
) -> (f64, u64) {
    if max_turn_input_tokens == 0 {
        return (0.0, 0);
    }
    let tokens = crate::prompts::estimate_tokens(messages, pinned_tool_schema_tokens, 0) as u64;
    (tokens as f64 / max_turn_input_tokens as f64, tokens)
}

// AgenticLoopState currently carries the effective input budget, not the full
// model context window. This approximation is used only for skill-listing size
// hints; callers with registry metadata should pass the full context window.
fn approximate_context_window_from_effective_input_budget(
    max_turn_input_tokens: u64,
) -> Option<u32> {
    if max_turn_input_tokens == 0 {
        return None;
    }
    let approx_context_window = max_turn_input_tokens.saturating_mul(10).div_ceil(8);
    Some(approx_context_window.min(u64::from(u32::MAX)) as u32)
}

/// Run the compaction pipeline and record results (event + audit).
///
/// Shared by pre-turn proactive compression and resume-time compression to
/// avoid ~30 lines of duplicated TokenBudget → pipeline selection → compress →
/// event → audit logic.
fn run_proactive_compaction<H: AgenticLoopHost>(
    pressure: f64,
    tokens_measured: u64,
    state: &mut AgenticLoopState,
    host: &mut H,
    kind: CompactionKind,
    audit_label: &str,
) {
    if state.compaction_effectiveness.is_circuit_open() {
        return;
    }

    let max_tokens = state.max_turn_input_tokens;
    let budget = TokenBudget {
        max_prompt_tokens: max_tokens,
        last_measured_tokens: tokens_measured,
        current_round_index: Some(state.current_round_index),
        now_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let pipeline = if pressure >= CompactionTier::aggressive_trigger(max_tokens) {
        CompactionEngine::aggressive_pipeline()
    } else {
        CompactionEngine::default_pipeline_for(max_tokens)
    };
    let messages_before = state.messages.len();
    let rewrite_permit = state.begin_canonical_rewrite();
    let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
    state
        .compaction_effectiveness
        .record_compaction(outcome.total_tokens_freed);
    if outcome.total_tokens_freed > 0 {
        state.finish_canonical_rewrite(rewrite_permit);
        let messages_after = state.messages.len();
        let messages_removed = messages_before.saturating_sub(messages_after);
        let layer_descriptions: Vec<String> = outcome
            .layer_results
            .iter()
            .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
            .collect();
        state.context_compression_triggered = true;
        state.step_recorder.record_compaction_with_kind(
            &kind.to_string(),
            messages_removed.min(u32::MAX as usize) as u32,
            outcome.total_tokens_freed,
            pressure,
        );
        let event = CompactionEvent::new(
            kind,
            pressure,
            outcome.total_tokens_freed,
            tokens_measured,
            max_tokens,
            messages_removed,
            messages_after,
            layer_descriptions,
        );
        host.on_compaction(event);
        if let Some(ref mut sess) = state.pipeline_session {
            sess.record_compaction_audit(
                audit_label,
                outcome.layer_results.len() as u32,
                outcome.total_tokens_freed.min(u32::MAX as u64) as u32,
            );
            sess.stats.record_compaction(outcome.total_tokens_freed);
        }
    }
}

pub(crate) async fn prepare_turn_iteration<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
) -> Result<PreparedTurnIteration, String> {
    let quiet = host.is_quiet();

    // Outer loop: re-evaluate pause/cancel state until we either get
    // cancelled or the run is no longer paused. This replaces the prior
    // recursive `Box::pin(prepare_turn_iteration(...)).await` which
    // would stack-overflow during long pause windows.
    loop {
        if wait_for_pause_clear_or_cancel(state).await {
            let origin = resolve_cancellation_origin(state).await;
            return Ok(finish_cancellation(host, state, origin).await);
        }

        // Fast-path in-memory cancel check (same pod)
        let in_memory_cancelled = state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        // Cross-pod durable control check (only if same-pod fast path did not
        // already catch cancellation). One lookup covers both cancel and pause
        // so between-turn control polling does not double DB load per run.
        let durable_control_status = if !in_memory_cancelled {
            if let Some(ref rc) = state.run_control {
                if let (Some(user_id), Some(run_id)) = (
                    state.context_manifest_user_id.as_deref(),
                    state.current_run_id.as_deref(),
                ) {
                    match rc.control_status(user_id, run_id).await {
                        Ok(status) => status,
                        Err(error) => {
                            tracing::warn!(
                                run_id,
                                error = %error,
                                "failed to poll run control status"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if in_memory_cancelled
            || matches!(durable_control_status, Some(RunControlStatus::Cancelled))
        {
            if matches!(durable_control_status, Some(RunControlStatus::Cancelled)) {
                if let Some(ref flag) = state.cancellation.flag {
                    flag.store(true, Ordering::SeqCst);
                }
                if let Some(ref token) = state.cancellation.token {
                    token.cancel();
                }
            }
            let origin = resolve_cancellation_origin(state).await;
            return Ok(finish_cancellation(host, state, origin).await);
        }

        if matches!(durable_control_status, Some(RunControlStatus::Paused)) {
            if let Some(ref flag) = state.cancellation.pause_flag {
                flag.store(true, Ordering::SeqCst);
            }
            // Loop back to top and re-enter the while-pause loop instead of
            // recursing (prevents stack overflow).
            continue;
        }

        // Clean state: no cancel, no pause — proceed to turn preparation.
        break;
    }

    // Reconcile before considering settlement so a task that demonstrably
    // entered implementation does not lose the implementation portion of its
    // initial slice merely because semantic admission was intentionally
    // deferred to the primary model.
    let _promoted_fallback_budget = promote_fallback_budget_after_observed_mutation(state);

    if state.remaining_turns == 0 {
        // Once the loop enters typed Work settlement, exploration is over.
        // Historical progress may justify extending an exploratory slice, but
        // it must never renew a settlement-only boundary: doing so would let a
        // failed or omitted settle_work_item call inherit credit from earlier
        // successful mutations and leave the carrier owned for another full
        // slice. Settlement has its own deterministic one-retry contract.
        if state.hooks.completion_settlement.work_settlement_only
            && !state.hooks.completion_settlement.text_only
        {
            if state.budget_wrapup_ignored_rounds == 0 {
                // The only advertised settlement capability either failed or
                // was not selected. Give the provider one clean retry without
                // reopening exploration.
                state.budget_wrapup_ignored_rounds = 1;
                state.max_turns = state.max_turns.saturating_add(1);
                state.remaining_turns = state.remaining_turns.saturating_add(1);
                state.push_volatile_payload(
                    super::host::VolatileKind::FinalAnswerSettlement,
                    serde_json::json!({
                        "schema": "work_settlement_required.v1",
                        "signal": "owned_work_attempt_unsettled",
                        "instruction": "Settle the currently owned WorkItem now with settle_work_item. Do not return prose and do not call any other tool.",
                        "authority": "canonical_work_lifecycle",
                    }),
                );
            } else {
                mark_work_settlement_incomplete(state);
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Completed,
                ));
            }
        } else if maybe_extend_turn_budget(state).is_some() {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "↻ Continuing — verified progress is still advancing.".to_string(),
                );
            }
            state.final_text.clear();
            state.interruption = None;
        } else if begin_budget_settlement(state) {
            let _cancelled = cancel_unfinished_child_agents(
                host,
                state,
                "parent execution is converging to its final response",
                CancellationOrigin::Runtime,
            )
            .await;
            state.final_text.clear();
            state.interruption = None;
        } else if let Some(action) = super::execution_phase::pending_completion_action(state)
            && !super::execution_phase::completion_action_window_is_batchable(state, &action)
        {
            state.final_text =
                "The requested verification chain could not be completed within one bounded settlement boundary; no completion claim is being made."
                    .to_string();
            state.final_text_streamed = false;
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::ExecutionIncomplete,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(
                    state,
                    Some("dependent verification hooks require sequential settlement".into()),
                ),
            ));
            try_write_heavy_checkpoint(state);
            return Ok(PreparedTurnIteration::Finished(
                AgenticLoopOutcome::Completed,
            ));
        } else if state.budget_wrapup_injected
            && state.hooks.completion_settlement.text_only
            && state.budget_wrapup_ignored_rounds == 1
        {
            // One provider ignored the hidden-tool boundary. Preserve the
            // existing one-retry contract without reopening any tool budget.
            state.max_turns = state.max_turns.saturating_add(1);
            state.remaining_turns = state.remaining_turns.saturating_add(1);
        } else if state.hooks.completion_settlement.text_only && !state.budget_wrapup_injected {
            // A typed settlement action consumed the prior boundary and asks
            // for final synthesis. This single text-only call closes the user
            // turn without reopening ordinary tools.
            state.max_turns = state.max_turns.saturating_add(1);
            state.remaining_turns = state.remaining_turns.saturating_add(1);
            state.hooks.completion_settlement.wrapup_origin = Some(BudgetWrapupOrigin::RoundSlice);
            state.budget_wrapup_injected = true;
            state.push_volatile_payload(
                super::host::VolatileKind::FinalAnswerSettlement,
                serde_json::json!({
                    "schema": "completion_settlement.v2",
                    "signal": "typed_completion_action_settled",
                    "mode": "text_only",
                    "allowed_action": serde_json::Value::Null,
                    "attempts_remaining": 0,
                    "declarations_may_remain_visible_for_cache": true,
                    "execution_authority": "none",
                    "instruction": "The bounded completion action has been attempted. Produce the final answer from the resulting evidence; do not request another tool.",
                    "authority": "typed_completion_action_window",
                }),
            );
        } else if should_complete_budget_exhaustion_gracefully(state) {
            try_write_heavy_checkpoint(state);
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::BudgetExhausted,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            let cancelled_agents = cancel_child_agents_with_timeout(
                host,
                state.runtime_tool_executor.clone(),
                unfinished_parallel_agent_ids(state),
                "owner execution boundary reached",
                CancellationOrigin::Runtime,
            )
            .await;
            state.final_text = budget_exhaustion_completion_text(state, &cancelled_agents);
            state.final_text_streamed = false;
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⚠ Turn budget exhausted — preserving progress and ending the turn.".into(),
                );
            }
            return Ok(PreparedTurnIteration::Finished(
                AgenticLoopOutcome::Completed,
            ));
        } else {
            return Err(format!(
                "{} (budget: {} turns)",
                CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG, state.max_turns
            ));
        }
    }

    reserve_budget_settlement_boundary(state);

    match state.rate_limit_cooldown.check_request(false) {
        astra_turn_core::rate_limit_cooldown::RateLimitAction::Proceed => {}
        astra_turn_core::rate_limit_cooldown::RateLimitAction::WaitAndRetry { delay_ms } => {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "⏳ Rate limit cooldown — waiting {:.1}s before next turn…",
                        delay_ms as f64 / 1000.0,
                    ),
                );
            }
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        astra_turn_core::rate_limit_cooldown::RateLimitAction::UseFallback { .. } => {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⏳ Rate limit cooldown — waiting 5s (no fallback model)…".into(),
                );
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        astra_turn_core::rate_limit_cooldown::RateLimitAction::Reject {
            reason,
            reset_in_ms,
        } => {
            let secs = reset_in_ms / 1000;
            if state.total_tool_calls > 0 {
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "⚠ Rate limit cooldown active ({}) — preserving {} tool call(s). Resets in {secs}s.",
                            reason.as_str(),
                            state.total_tool_calls,
                        ),
                    );
                }
                state.final_text = format!(
                    "[Rate limit cooldown active ({}). \
                     {} completed tool call(s) preserved. \
                     Cooldown resets in ~{secs}s — you can continue then.]\n",
                    reason.as_str(),
                    state.total_tool_calls,
                );
                state.final_text_streamed = false;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::CooldownRejected,
                    ResumeAction::WaitAndRetry {
                        delay_seconds: reset_in_ms / 1000,
                    },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit: {}", reason.as_str())),
                    ),
                ));
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Completed,
                ));
            }
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::CooldownRejected,
                ResumeAction::WaitAndRetry {
                    delay_seconds: reset_in_ms / 1000,
                },
                interruption_state_summary(state, Some(format!("Rate limit: {}", reason.as_str()))),
            ));
            return Err(format!(
                "Rate limit cooldown active ({}). Resets in ~{secs}s. Please wait and retry.",
                reason.as_str(),
            ));
        }
    }

    state.remaining_turns = state.remaining_turns.saturating_sub(1);
    state
        .step_recorder
        .begin_turn_with_context(session_turn_number(state), turn_index as u32);

    if let Some(ref mut adapter) = state.tactical_adapter {
        adapter.reset_turn();
    }
    if let Some(ref mut collector) = state.step_signal_collector {
        collector.reset(state.max_turn_input_tokens);
    }

    let turn_start_time = Instant::now();
    tracing::debug!(
        target: "astra_timing",
        turn_index = turn_index,
        session_turn = session_turn_number(state),
        "agentic turn started"
    );
    if state.turn_event_buffer.is_none() {
        state.turn_event_buffer = Some(
            astra_services::session_journal::TurnEventBuffer::begin_turn(
                state.current_session_id.as_deref(),
                session_turn_number(state),
            ),
        );
    }

    if let (Some(hub), Some(session)) = (
        &state.telemetry.observability_hub,
        &state.telemetry.observability_session,
    ) {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_id = {
            let s = astra_core::sync_poison::recover_rwlock_read(session);
            s.user_id.clone()
        };
        crate::observability::on_turn_start(hub, session_id, &user_id, &state.message);
    }
    // TurnIntent describes the submitted user turn, not an individual model
    // round. Re-running an auxiliary judge after every tool round adds
    // unbounded latency and can overwrite the semantics that governed the
    // first round. A resumed non-zero round preserves the restored state.
    if turn_index == 0 {
        let admission_started_at = Instant::now();
        let outcome = host.judge_turn_intent(state).await;
        // This is emitted before an unavailable admission can terminate the
        // turn, so failed slow starts remain diagnosable. The common fanout
        // helper guarantees trace, log, and Explain share one duration.
        complete_turn_phase(
            host,
            state,
            admission_started_at,
            TurnPhaseKind::SemanticAdmission,
            0,
            0,
            TurnPhaseOutcome::from(&outcome),
            "turn_intent_admission_0".to_string(),
        );
        match outcome {
            TurnIntentJudgeOutcome::Intent(intent) => {
                let record_feedback = record_current_user_turn_semantics(state, &intent);
                apply_judged_turn_intent_to_observability(state, &intent, record_feedback);
                apply_judged_turn_intent_to_runtime_profile(state, &intent);
                if record_feedback && intent.reanchors_current_objective() {
                    apply_structured_user_reanchor(state, intent.objective_relation);
                }
                if record_feedback {
                    apply_structured_user_feedback(state, &intent);
                }
            }
            TurnIntentJudgeOutcome::FixedDefault => {
                // The baseline profile is a text-independent fixed default. Do not
                // retain or synthesize judge-owned semantics without a typed LLM
                // result.
                state.turn_intent = None;
            }
            TurnIntentJudgeOutcome::Delegated => {
                // A transport adapter (for example the CLI Server bridge)
                // records the phase locally but leaves the semantic decision
                // to the authoritative Server turn. Do not label this as a
                // fixed default or manufacture local intent.
                state.turn_intent = None;
            }
            TurnIntentJudgeOutcome::Unavailable => {
                if host.requires_turn_intent_decision() {
                    return Err(
                        "semantic task admission is temporarily unavailable; primary execution was not started, so retrying cannot bypass the canonical Work lifecycle"
                            .to_string(),
                    );
                }
                tracing::debug!(
                    "turn intent judge unavailable; preserving current runtime profile"
                );
            }
        }
    }
    if let Some(prompt) =
        astra_turn_core::stop_hooks::build_stop_hook_prompt(&state.hooks.stop_hooks)
        && let Some(content) = prompt.get("content").and_then(Value::as_str)
    {
        state.push_volatile(
            super::host::VolatileKind::StopHookEvidence,
            content.to_string(),
        );
    }

    if (state.telemetry.observability_session.is_some() || state.skills.resolver.is_some())
        && state.telemetry.turn_trace_collector.is_none()
    {
        let capture = std::env::var("ASTRA_CAPTURE_TRACES")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);
        if capture {
            let turn_id = format!("turn-{}", turn_index);
            let session_id = state.current_session_id.clone().unwrap_or_default();
            state.telemetry.turn_trace_collector = Some(
                crate::turn::turn_trace_collector::TurnTraceCollector::new(turn_id, session_id),
            );
        }
    }

    if state.permission_handler.is_none()
        && let Some(ctx) = state.permission_context.clone()
    {
        state.permission_handler = Some(crate::orchestration::PermissionRequestHandler::new(ctx));
    }

    const MAX_MAILBOX_DRAIN_PER_TURN: usize = 64;
    if let Some(ref mut mailbox) = state.messaging.mailbox {
        let (pending, has_more) = mailbox.drain_bounded(MAX_MAILBOX_DRAIN_PER_TURN);
        if !pending.is_empty() {
            let mut parts = Vec::with_capacity(pending.len());
            let mut delivered_count = 0usize;
            for msg in &pending {
                // Transport-level broadcast groups include every subscriber,
                // including the sender. `broadcast` is a peer-coordination
                // intent, so consume/ack the sender echo without reinjecting
                // its own guidance or generating a meaningless self-ack.
                if msg.from == mailbox.address
                    && matches!(
                        msg.to,
                        astra_messaging::types::MessageTarget::Broadcast { .. }
                    )
                {
                    continue;
                }
                let from_label = &msg.from.agent_id;
                let is_transient_progress = matches!(
                    msg.payload,
                    astra_messaging::types::MessagePayload::Progress { .. }
                );
                if !is_transient_progress {
                    delivered_count += 1;
                    host.on_agent_communication(astra_messaging::agent_communication_event(
                        &mailbox.address,
                        astra_messaging::AgentCommunicationDirection::Received,
                        msg,
                    ));
                }

                match &msg.payload {
                    astra_messaging::types::MessagePayload::Ack { message_id } => {
                        if let Some(ref tracker) = state.messaging.ack_tracker {
                            tracker.acknowledge(message_id).await;
                        }
                        if let Some(ref metrics) = state.messaging.metrics {
                            metrics.acks_received.fetch_add(1, Ordering::Relaxed);
                        }
                        parts.push(format!(
                            "[{from_label} applied]: message {message_id} reached the receiver model boundary"
                        ));
                        continue;
                    }
                    astra_messaging::types::MessagePayload::Nack { message_id, reason } => {
                        if let Some(ref tracker) = state.messaging.ack_tracker
                            && let Some(astra_messaging::ack_tracker::AckOutcome::Rejected {
                                message,
                                ..
                            }) = tracker.reject(message_id, reason.clone()).await
                        {
                            eprintln!(
                                "  ⚠ messaging: nack for message {}: {}",
                                message_id,
                                reason.as_deref().unwrap_or("no reason")
                            );
                            if let Some(ref dlq) = state.messaging.dead_letter_queue {
                                dlq.store(
                                    Arc::clone(&message),
                                    astra_messaging::dead_letter::DeadLetterReason::Rejected {
                                        reason: reason.clone(),
                                    },
                                    1,
                                )
                                .await;
                            }
                            if let Some(ref metrics) = state.messaging.metrics {
                                metrics.dead_letters.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if let Some(ref metrics) = state.messaging.metrics {
                            metrics.nacks_received.fetch_add(1, Ordering::Relaxed);
                        }
                        let r = reason.as_deref().unwrap_or("no reason");
                        parts.push(format!(
                            "[{from_label} nack]: message {message_id} rejected — {r}"
                        ));
                        continue;
                    }
                    _ => {}
                }

                if let Some(ref metrics) = state.messaging.metrics {
                    metrics.messages_received.fetch_add(1, Ordering::Relaxed);
                }

                if msg.requires_ack {
                    let ack_reply = msg.make_ack(mailbox.address.clone());
                    if let Err(e) = mailbox.send(ack_reply.clone()).await {
                        astra_core::agent_warn!("mailbox", "Failed to send ack: {e}");
                    } else {
                        host.on_agent_communication(astra_messaging::agent_communication_event(
                            &mailbox.address,
                            astra_messaging::AgentCommunicationDirection::Sent,
                            &ack_reply,
                        ));
                    }
                    if let Some(ref metrics) = state.messaging.metrics {
                        metrics.acks_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if let Some(ref handler) = state.permission_handler
                    && let Some((correlation_id, response)) = handler.process_message(msg).await
                {
                    let response_msg =
                        response.to_message(&mailbox.address, &msg.from, &correlation_id);
                    if let Err(e) = mailbox.send(response_msg.clone()).await {
                        astra_core::agent_warn!(
                            "mailbox",
                            "Failed to send permission response: {e}"
                        );
                    } else {
                        host.on_agent_communication(astra_messaging::agent_communication_event(
                            &mailbox.address,
                            astra_messaging::AgentCommunicationDirection::Sent,
                            &response_msg,
                        ));
                    }
                    continue;
                }

                match &msg.payload {
                    astra_messaging::types::MessagePayload::Text { content, .. } => {
                        push_mailbox_model_preview(
                            &mut parts,
                            format!("[{from_label}]: {content}"),
                        );
                    }
                    astra_messaging::types::MessagePayload::Progress { .. } => {}
                    astra_messaging::types::MessagePayload::Request { request_type, data } => {
                        let data = (!data.is_null()).then(|| format!(" · {data}"));
                        push_mailbox_model_preview(
                            &mut parts,
                            format!(
                                "[{from_label} request]: {request_type:?}{}",
                                data.as_deref().unwrap_or("")
                            ),
                        );
                    }
                    astra_messaging::types::MessagePayload::Response {
                        request_id,
                        accepted,
                        data,
                    } => {
                        let data = data.as_ref().map(|data| format!(" · {data}"));
                        push_mailbox_model_preview(
                            &mut parts,
                            format!(
                                "[{from_label} response to {request_id}]: accepted={accepted}{}",
                                data.as_deref().unwrap_or("")
                            ),
                        );
                    }
                    astra_messaging::types::MessagePayload::Signal(sig) => {
                        push_mailbox_model_preview(
                            &mut parts,
                            format!("[{from_label} signal]: {sig:?}"),
                        );
                    }
                    astra_messaging::types::MessagePayload::Ack { .. } => {}
                    astra_messaging::types::MessagePayload::Nack { .. } => {}
                }
            }
            if let Err(error) = mailbox.acknowledge_received(&pending).await {
                astra_core::agent_warn!(
                    "mailbox",
                    "failed to confirm consumed messages; transport may redeliver them: {error}"
                );
            }
            if !parts.is_empty() {
                let mailbox_text = format!(
                    "📬 Messages from other agents ({}{}):\n{}",
                    delivered_count,
                    if has_more { "+, more queued" } else { "" },
                    parts.join("\n")
                );
                state.push_volatile(super::host::VolatileKind::Mailbox, mailbox_text);
            }
        }
    }

    if let Some(resolver) = &state.skills.resolver {
        // Phase-9: skill listing moves from per-turn volatile to
        // session-stable. The full skill catalog is rendered via
        // CacheScope::Session — the resolved turn input budget sizes the
        // listing so small-context providers do not waste prompt space and
        // large-context providers are not silently capped by model-name
        // fallbacks.
        //
        // We still populate `listing_message` as a rendered `role: system`
        // value for downstream adapters (introspect tooling, tests) so
        // they don't need to know about the cache-scope plumbing.
        let full = resolver.available_skills();
        if state.skills.listing_message.is_none() {
            state.skills.listing_message = if full.is_empty() {
                None
            } else {
                let agent_spawn_available = host
                    .capabilities()
                    .has(astra_turn_core::capability::Capability::AgentSpawner);
                let edge_skills = full
                    .iter()
                    .map(|skill| {
                        serde_json::json!({
                            "name": skill.name,
                            "version": null,
                            "description": skill.description,
                            "when_to_use": skill.when_to_use,
                            "aliases": skill.aliases,
                        })
                    })
                    .collect::<Vec<_>>();
                crate::prompts::build_skill_listing_section_with_context_window_and_caps(
                    &full,
                    approximate_context_window_from_effective_input_budget(
                        state.max_turn_input_tokens,
                    ),
                    agent_spawn_available,
                )
                .map(|section| {
                    serde_json::json!({
                        "role": "system",
                        "content": section.text,
                        "edge_skills": edge_skills,
                    })
                })
            };
        }
    }
    maybe_pre_route_skill(host, state).await;

    if turn_index > 0 {
        // ── Stall correction: inject a nudge if stall was detected ────
        // Stall events are recorded during the tool phase of the *previous*
        // turn.  If any new events appeared, build a reflection and inject it
        // so the LLM can self-correct before the next tool round.
        //
        // Limit: at most 3 nudges per loop to avoid nudge-spam which itself
        // wastes context.
        const MAX_NUDGES: u32 = 3;
        if !state.stall.events.is_empty() && state.stall.nudge_count < MAX_NUDGES {
            let recent_events: Vec<_> = state
                .stall
                .events
                .iter()
                .filter(|(_, t)| *t as usize >= turn_index.saturating_sub(1))
                .collect();
            if !recent_events.is_empty() {
                let error_tools: Vec<&str> = state.turn_guard.health.health_avoidance_tools();
                let reflection = astra_turn_core::stall::build_stall_reflection(
                    &state.stall.turn_sigs,
                    &error_tools,
                    state.stall.nudge_count as usize,
                );
                let nudge = reflection.to_nudge_message();
                state.push_volatile(super::host::VolatileKind::StallNudge, nudge);
                state.stall.nudge_count += 1;
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "  ⚠ Stall correction injected (nudge #{}) — {}",
                            state.stall.nudge_count, reflection.what_happened,
                        ),
                    );
                }
            }
        }

        // Context pressure estimation + adaptive compaction.
        // When pipeline_session is active, use its pressure model (predictive
        // with reserves) and cascade-aware limits. Otherwise fall back to
        // legacy inline estimation.
        let (mut pressure, mut pressure_estimate_tokens) = estimate_context_pressure(
            &state.messages,
            state.pinned_tool_schema_tokens as usize,
            state.max_turn_input_tokens,
        );

        // Pre-turn LLM compact: if pressure exceeds the model-adaptive
        // trigger, let the host run an optional cache-friendly inline-summary
        // pass before the next LLM call.
        if pressure >= CompactionTier::pre_turn_trigger(state.max_turn_input_tokens)
            && state.compact_tier_applied < CompactionTier::CompactHistory
            && state.messages.len() > 10
        {
            if let Some(event) = host.maybe_pre_turn_compact(state, pressure).await {
                state.context_compression_triggered = true;
                state
                    .compaction_effectiveness
                    .record_compaction(event.tokens_freed);
                state.step_recorder.record_compaction_with_kind(
                    &event.kind.to_string(),
                    event.messages_removed.min(u32::MAX as usize) as u32,
                    event.tokens_freed,
                    event.pressure,
                );
                if let Some(ref mut sess) = state.pipeline_session {
                    let audit_label = event.kind.to_string();
                    sess.record_compaction_audit(
                        &audit_label,
                        event.messages_removed.min(u32::MAX as usize) as u32,
                        event.tokens_freed.min(u64::from(u32::MAX)) as u32,
                    );
                    sess.stats.record_compaction(event.tokens_freed);
                }
                host.on_compaction(event);
                (pressure, pressure_estimate_tokens) = estimate_context_pressure(
                    &state.messages,
                    state.pinned_tool_schema_tokens as usize,
                    state.max_turn_input_tokens,
                );
            }
        }

        // Pre-turn pressure warning: when context is near the model-adaptive
        // warning threshold, emit a non-intrusive advisory so the user knows
        // compaction is imminent.
        if pressure >= CompactionTier::pre_turn_warning(state.max_turn_input_tokens) {
            let warning = CompactionEvent::new(
                CompactionKind::PressureWarning,
                pressure,
                0, // no tokens freed yet
                pressure_estimate_tokens,
                state.max_turn_input_tokens,
                0,                    // no messages removed
                state.messages.len(), // current message count
                vec![],               // no layers applied
            );
            host.on_compaction(warning);
        }

        // Adaptive microcompact: scale aggressiveness with context pressure.
        // When pipeline_session is active, cascade detection suppresses clearing
        // to break infinite compaction loops.
        let pipeline_allows_clearing = state
            .pipeline_session
            .as_ref()
            .map(|sess| !sess.stats.has_compaction_cascade())
            .unwrap_or(true);

        let strategy = state.compact_strategy;
        let session_dir = state.current_session_id.as_deref().and_then(|sid| {
            match state.context_manifest_user_id.as_deref() {
                Some(user_id) => astra_services::OwnerScope::user(user_id).and_then(|owner| {
                    astra_services::local_session_artifact_store()
                        .session_dir_for_owner(&owner, sid)
                }),
                None => astra_services::local_session_artifact_store().session_dir(sid),
            }
            .ok()
        });
        let rewrite_permit = state.begin_canonical_rewrite();
        let mc = if !pipeline_allows_clearing {
            // Cascade detected: skip clearing this turn to break the loop.
            astra_turn_core::microcompact::CompactStats::default()
        } else if !state.session_facts.active_files.is_empty() {
            astra_turn_core::microcompact::compact_tool_results_state_aware_with_persistence_protected_prefix(
                &mut state.messages,
                pressure,
                &state.session_facts,
                5,
                strategy,
                session_dir.as_deref(),
                state.last_request_message_count,
            )
        } else {
            astra_turn_core::microcompact::compact_tool_results_adaptive_with_persistence_protected_prefix(
                &mut state.messages,
                pressure,
                strategy,
                session_dir.as_deref(),
                state.last_request_message_count,
            )
        };
        if mc.results_compacted > 0 {
            state.finish_canonical_rewrite(rewrite_permit);
            state.context_compression_triggered = true;
            let event = CompactionEvent::new(
                CompactionKind::Microcompact,
                pressure,
                mc.tokens_saved as u64,
                pressure_estimate_tokens,
                state.max_turn_input_tokens,
                mc.results_compacted,
                state.messages.len(),
                vec![format!("microcompact: ~{} tokens", mc.tokens_saved)],
            );
            host.on_compaction(event);
            state.step_recorder.record_compaction_with_kind(
                "microcompact",
                mc.results_compacted as u32,
                mc.tokens_saved as u64,
                pressure,
            );
            if let Some(ref mut sess) = state.pipeline_session {
                sess.record_compaction_audit(
                    "tool_result_clearing",
                    mc.results_compacted.min(u32::MAX as usize) as u32,
                    mc.tokens_saved.min(u32::MAX as usize) as u32,
                );
                sess.stats.record_compaction(mc.tokens_saved as u64);
            }
        }

        // Re-estimate pressure after microcompact (messages may have shrunk).
        // Use the same precise estimation as the pre-microcompact check
        // above so that always-load tool schema tokens are included — without
        // them, the guard under-estimates pressure and skips compaction
        // (observed in session 540c37d1 where budget_pressure=0.887 but
        // post_mc_pressure was ~0.61 and never crossed the 0.75 threshold).
        let (post_mc_pressure, post_mc_tokens) = estimate_context_pressure(
            &state.messages,
            state.pinned_tool_schema_tokens as usize,
            state.max_turn_input_tokens,
        );

        // Proactive compression gate: if pressure is still high after
        // microcompact, run the full compression pipeline *before* calling
        // the LLM, preventing 413 errors instead of reacting to them.
        if post_mc_pressure >= CompactionTier::pre_turn_trigger(state.max_turn_input_tokens) {
            let is_aggressive =
                post_mc_pressure >= CompactionTier::aggressive_trigger(state.max_turn_input_tokens);
            let (kind, label) = if is_aggressive {
                (CompactionKind::ProactiveAggressive, "aggressive")
            } else {
                (CompactionKind::ProactiveDefault, "default")
            };
            run_proactive_compaction(post_mc_pressure, post_mc_tokens, state, host, kind, label);
        }
    }

    // ── Compaction-on-resume: if turn 0 has many messages (restored from
    // checkpoint), estimate context pressure from raw content size and
    // proactively compress before the first LLM call.  This prevents an
    // immediate 413 when resuming from a CompactAndRetry interruption.
    if turn_index == 0 && state.messages.len() > 10 && state.max_turn_input_tokens > 0 {
        let (estimated_pressure, estimated_tokens) = estimate_context_pressure(
            &state.messages,
            state.pinned_tool_schema_tokens as usize,
            state.max_turn_input_tokens,
        );
        if estimated_pressure >= CompactionTier::pre_turn_trigger(state.max_turn_input_tokens) {
            run_proactive_compaction(
                estimated_pressure,
                estimated_tokens,
                state,
                host,
                CompactionKind::Resume,
                "resume",
            );
        }
    }

    Ok(PreparedTurnIteration::Ready(TurnIterationPrep {
        quiet,
        turn_start_time,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astra_config::user_profile::{Scenario, TurnIntent, WorkspaceMutationIntent};
    use astra_services::session_journal::ToolCallRecord;
    use astra_skills::hooks::SkillHooks;
    use astra_skills::manifest::{ExecutionContext, SkillSourceKind, TrustTier};
    use astra_skills::traits::{ResolvedSkill, SkillResolver, SkillToolInfo};
    use serde_json::json;

    use crate::turn::agentic_loop::host::VolatileKind;
    use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
    use crate::turn::agentic_loop::host::tests::{
        MockHost, make_hub, make_session, make_state, text_result,
    };

    use super::*;

    #[test]
    fn runtime_decision_intent_does_not_reinterpret_user_text() {
        let message = "review literal <system-reminder> syntax";

        assert_eq!(
            pure_user_intent_for_runtime_decision(message),
            "review literal <system-reminder> syntax"
        );
    }

    #[tokio::test]
    async fn loop_preamble_starts_fresh_compression_tracking() {
        let mut state = make_state();
        state.context_compression_triggered = true;
        let mut host = MockHost::new(Vec::new());

        run_loop_preamble(&mut host, &mut state).await;

        assert!(!state.context_compression_triggered);
    }

    fn agent_record(
        action: &str,
        args: serde_json::Value,
        result: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "agent".into(),
            ok: error.is_none(),
            ms: 0,
            error: error.map(str::to_string),
            args_preview: Some(action.to_string()),
            args_full: Some(args.to_string()),
            result_full: result.map(|value| value.to_string()),
            ..Default::default()
        }
    }

    fn read_record(round: u32, start_line: u32, end_line: u32) -> ToolCallRecord {
        read_record_at_path(round, "/workspace/sample.rs", start_line, end_line)
    }

    fn read_record_at_path(
        round: u32,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            file_path: Some(path.into()),
            args_full: Some(
                json!({
                    "path": path,
                    "start_line": start_line,
                    "end_line": end_line
                })
                .to_string(),
            ),
            result_full: Some(format!("lines {start_line}-{end_line}")),
            round: Some(round),
            ..Default::default()
        }
    }

    fn write_record(round: u32, path: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "write_file".into(),
            ok,
            file_path: Some(path.into()),
            args_full: Some(json!({"path": path, "content": "updated"}).to_string()),
            round: Some(round),
            ..Default::default()
        }
    }

    /// Production-shaped successful structured writer. Tests that exercise
    /// completion, renewal, or validation after an actual commit must use the
    /// executor-owned receipt; `write_record` remains available for risk-only
    /// and negative-evidence fixtures.
    fn committed_write_record(round: u32, path: &str) -> ToolCallRecord {
        let mut record = write_record(round, path, true);
        let receipt = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        record.workspace_mutation_observed = Some(true);
        record.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        record.workspace_mutation_receipt = receipt
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        record
    }

    #[test]
    fn observed_stable_mutation_promotes_only_the_unclassified_default_slice() {
        let mut state = make_state();
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.task_profile = fallback;
        state.agentic_turn_budget = fallback.agentic_turn_budget;
        state.max_turns = fallback.agentic_turn_budget.initial_turns;
        state.remaining_turns = 0;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![committed_write_record(3, "/workspace/src/lib.rs")];

        assert!(promote_fallback_budget_after_observed_mutation(&mut state));
        assert_eq!(
            state.max_turns,
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            )
            .agentic_turn_budget
            .initial_turns
        );
        assert_eq!(state.remaining_turns, 8);
        assert_eq!(
            state.task_profile, fallback,
            "an observed write is not semantic intent"
        );
    }

    #[test]
    fn authoritative_mutation_promotes_fallback_initial_slice_once_without_widening_ceiling() {
        let mut state = make_state();
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.task_profile = fallback;
        state.agentic_turn_budget = fallback.agentic_turn_budget;
        state.max_turns = fallback.agentic_turn_budget.initial_turns;
        state.remaining_turns = 19;
        let original_hard_limit = state.agentic_turn_budget.hard_turn_limit;
        let original_extension_turns = state.agentic_turn_budget.extension_turns;
        let original_max_extensions = state.agentic_turn_budget.max_extensions;

        assert!(promote_fallback_budget_for_authoritative_mutation(
            &mut state
        ));
        assert_eq!(state.max_turns, 32);
        assert_eq!(state.remaining_turns, 27);
        assert_eq!(
            state.agentic_turn_budget.hard_turn_limit,
            original_hard_limit
        );
        assert_eq!(
            state.agentic_turn_budget.extension_turns,
            original_extension_turns
        );
        assert_eq!(
            state.agentic_turn_budget.max_extensions,
            original_max_extensions
        );
        assert!(
            !promote_fallback_budget_for_authoritative_mutation(&mut state),
            "the same authoritative admission must not mint another slice"
        );

        let mut explicit = make_state();
        explicit.task_profile = fallback;
        explicit.agentic_turn_budget = fallback.agentic_turn_budget;
        explicit.max_turns = fallback.agentic_turn_budget.initial_turns;
        explicit.budget_is_explicit = true;
        assert!(!promote_fallback_budget_for_authoritative_mutation(
            &mut explicit
        ));

        let mut already_classified = make_state();
        already_classified.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        already_classified.agentic_turn_budget = fallback.agentic_turn_budget;
        already_classified.max_turns = fallback.agentic_turn_budget.initial_turns;
        assert!(!promote_fallback_budget_for_authoritative_mutation(
            &mut already_classified
        ));
    }

    #[test]
    fn observed_mutation_never_overrides_explicit_or_untrusted_budget_context() {
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        let mut explicit = make_state();
        explicit.task_profile = fallback;
        explicit.agentic_turn_budget = fallback.agentic_turn_budget;
        explicit.max_turns = fallback.agentic_turn_budget.initial_turns;
        explicit.hooks.workspace_root_hint = Some("/workspace".into());
        explicit.stall.tool_call_records = vec![write_record(3, "/workspace/src/lib.rs", true)];
        explicit.budget_is_explicit = true;
        assert!(!promote_fallback_budget_after_observed_mutation(
            &mut explicit
        ));

        let mut failed = make_state();
        failed.task_profile = fallback;
        failed.agentic_turn_budget = fallback.agentic_turn_budget;
        failed.max_turns = fallback.agentic_turn_budget.initial_turns;
        failed.hooks.workspace_root_hint = Some("/workspace".into());
        failed.stall.tool_call_records = vec![write_record(3, "/workspace/src/lib.rs", false)];
        assert!(!promote_fallback_budget_after_observed_mutation(
            &mut failed
        ));

        let mut scratch = make_state();
        scratch.task_profile = fallback;
        scratch.agentic_turn_budget = fallback.agentic_turn_budget;
        scratch.max_turns = fallback.agentic_turn_budget.initial_turns;
        scratch.hooks.workspace_root_hint = Some("/workspace".into());
        scratch.stall.tool_call_records = vec![write_record(3, "/tmp/scratch.rs", true)];
        assert!(!promote_fallback_budget_after_observed_mutation(
            &mut scratch
        ));
    }

    #[test]
    fn observed_mutation_promotion_preserves_the_resolved_runtime_ceiling() {
        let mut state = make_state();
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.task_profile = fallback;
        state.agentic_turn_budget = fallback.agentic_turn_budget;
        state.agentic_turn_budget.hard_turn_limit = 28;
        state.agentic_turn_budget.extension_turns = 4;
        state.agentic_turn_budget.max_extensions = 1;
        state.max_turns = 24;
        state.remaining_turns = 0;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![committed_write_record(3, "/workspace/src/lib.rs")];

        assert!(promote_fallback_budget_after_observed_mutation(&mut state));
        assert_eq!(state.max_turns, 28);
        assert_eq!(state.remaining_turns, 4);
        assert_eq!(state.agentic_turn_budget.hard_turn_limit, 28);
        assert_eq!(state.agentic_turn_budget.extension_turns, 4);
        assert_eq!(state.agentic_turn_budget.max_extensions, 1);
    }

    #[tokio::test]
    async fn observed_mutation_promotes_before_budget_settlement() {
        let mut state = make_state();
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.task_profile = fallback;
        state.agentic_turn_budget = fallback.agentic_turn_budget;
        state.max_turns = fallback.agentic_turn_budget.initial_turns;
        state.remaining_turns = 0;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![committed_write_record(23, "/workspace/src/lib.rs")];
        let mut host = MockHost::new(Vec::new());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 24)
            .await
            .expect("a promoted slice remains runnable");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.max_turns, 32);
        assert_eq!(
            state.remaining_turns, 7,
            "the returned iteration has reserved one of the promoted turns"
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
    }

    fn bash_record(round: u32, command: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok,
            args_full: Some(json!({"command": command}).to_string()),
            round: Some(round),
            ..Default::default()
        }
    }

    /// Production-shaped shell mutation whose invocation-cgroup owner observed
    /// a change inside the bound workspace. Lexical command shape alone is not
    /// durable mutation authority.
    fn authoritative_bash_mutation_record(round: u32, command: &str) -> ToolCallRecord {
        let mut record = bash_record(round, command, true);
        let receipt = astra_tools::workspace_observation::changed_receipt();
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        record.runtime_args_full = record.args_full.clone();
        record.workspace_mutation_observed = Some(true);
        record.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        record.workspace_mutation_receipt = receipt
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        record
    }

    #[test]
    fn explicit_bash_verification_receipt_requires_the_exact_contract() {
        let receipt = astra_tools::workspace_observation::explicit_workspace_verification_receipt();
        let mut record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(json!({"command": "pytest -q", "mode": "verify"}).to_string()),
            runtime_args_full: Some(json!({"command": "pytest -q", "mode": "verify"}).to_string()),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: receipt
                .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .cloned(),
            ..Default::default()
        };
        assert!(record_has_typed_workspace_observation_receipt(&record));

        record.args_full = Some(json!({"command": "pytest -q"}).to_string());
        record.runtime_args_full = record.args_full.clone();
        assert!(!record_has_typed_workspace_observation_receipt(&record));

        record.args_full = Some(json!({"command": "pytest -q", "mode": "verify"}).to_string());
        record.runtime_args_full = record.args_full.clone();
        record.workspace_mutation_receipt = Some(json!({
            "schema": "workspace_observation_receipt.v2",
            "source": "model_claim",
            "scope": astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
            "ownership": "typed_workspace_observer",
        }));
        assert!(!record_has_typed_workspace_observation_receipt(&record));
    }

    #[test]
    fn record_path_uses_live_arguments_before_redacted_projection() {
        let raw_path = "/workspace/password=super_secret_value_123456";
        let mut record = ToolCallRecord {
            name: "read_file".into(),
            file_path: Some("[REDACTED:SECRET_ASSIGNMENT]".into()),
            args_full: Some(json!({"path": "[REDACTED:SECRET_ASSIGNMENT]"}).to_string()),
            runtime_args_full: Some(json!({"path": raw_path}).to_string()),
            ..Default::default()
        };
        assert_eq!(record_explicit_path(&record).as_deref(), Some(raw_path));

        record.runtime_args_full = None;
        assert_eq!(record_explicit_path(&record), None);
    }

    fn explicit_verification_hook(
        label: &str,
        command: &str,
    ) -> astra_turn_core::stop_hooks::StopHook {
        astra_turn_core::stop_hooks::StopHook {
            label: label.into(),
            command: command.into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
            authoritative: true,
        }
    }

    fn turn_sig(signature: &str) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([signature.to_string()])
    }

    #[test]
    fn budget_extension_accepts_successful_read_only_analysis_with_distinct_turns() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("分析session");
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 80),
            read_record_at_path(1, "/workspace/second.rs", 81, 160),
        ];
        state.stall.turn_sigs = vec![turn_sig("read_file:first"), turn_sig("read_file:second")];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "successful non-repetitive read-only analysis should be eligible for continuation"
        );
    }

    #[test]
    fn budget_extension_stops_when_workspace_observation_is_quarantined() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(temp.path())
        );
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(temp.path().to_string_lossy().into_owned());
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 20),
            read_record_at_path(1, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![turn_sig("read:first"), turn_sig("read:second")];

        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_does_not_use_read_observations_after_unstructured_write() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/out.txt"),
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("write:out"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_allows_unstructured_write_after_canonical_validation() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/out.txt"),
            bash_record(1, "cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:out"), turn_sig("test:workspace")];

        assert!(!state.task_profile.mutates_workspace);
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_keeps_rejected_writer_in_read_only_mode() {
        let mut state = make_state();
        let mut rejected = write_record(0, "/workspace/out.txt", false);
        rejected.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        state.stall.tool_call_records = vec![
            rejected,
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("rejected:write"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_does_not_use_reads_after_an_unknown_executed_writer() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            bash_record(
                0,
                "python3 -c \"open('/workspace/out.txt','w').write('x')\"",
                true,
            ),
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("unknown-write"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_does_not_use_reads_after_a_failed_direct_writer() {
        let mut state = make_state();
        let mut failed = write_record(0, "/workspace/out.txt", false);
        failed.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        state.stall.tool_call_records = vec![
            failed,
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("failed:write"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_allows_reads_after_a_successful_canonical_validator() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            bash_record(0, "cargo test", true),
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("test:workspace"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn workspace_validation_must_stay_inside_the_bound_root() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());

        for command in [
            "cd /tmp/unrelated && cargo test",
            "cargo test --manifest-path /tmp/unrelated/Cargo.toml",
            "cargo test --manifest-path=/tmp/unrelated/Cargo.toml",
            "python3 -m pytest /tmp/unrelated/tests",
            "python3 -m pytest \"$TMPDIR/other/tests\"",
            "cd \"$TMPDIR/other\" && cargo test",
        ] {
            state.stall.tool_call_records = vec![
                committed_write_record(0, "/workspace/out.txt"),
                bash_record(1, command, true),
            ];
            state.stall.turn_sigs = vec![turn_sig("write"), turn_sig(command)];
            assert!(
                !recent_activity_supports_budget_extension(&state),
                "external validator must not renew the workspace task: {command}"
            );
            assert_eq!(
                super::super::execution_phase::pending_completion_action(&state),
                Some(super::super::host::CompletionAction::PostMutationObservation),
                "external validator must not close the workspace evidence epoch: {command}"
            );
        }
    }

    #[test]
    fn metadata_only_validator_does_not_extend_a_mutation_epoch() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        for command in [
            "cargo test --help",
            "pytest --help",
            "cargo test '--help'",
            "cargo test \"$MODE\"",
            "pytest \"$MODE\"",
        ] {
            state.stall.tool_call_records = vec![
                committed_write_record(0, "/workspace/out.txt"),
                bash_record(1, command, true),
            ];
            state.stall.turn_sigs = vec![turn_sig("write"), turn_sig(command)];
            assert!(
                !recent_activity_supports_budget_extension(&state),
                "metadata-only command must not renew a mutation epoch: {command}"
            );
        }
    }

    #[test]
    fn timed_concurrent_writer_cannot_close_a_workspace_mutation_epoch() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        for command in [
            "time cp /workspace/source /workspace/out | cargo test",
            "time cat /workspace/source > /workspace/out | cargo test",
            "cd /workspace > /workspace/out | cargo test",
            "set -e > /workspace/out | cargo test",
        ] {
            state.stall.tool_call_records = vec![
                committed_write_record(0, "/workspace/initial.rs"),
                bash_record(1, command, true),
            ];
            state.stall.turn_sigs = vec![turn_sig("write"), turn_sig(command)];
            assert!(
                !recent_activity_supports_budget_extension(&state),
                "an opaque timed pipeline must not manufacture post-mutation evidence: {command}"
            );
            assert_eq!(
                super::super::execution_phase::pending_completion_action(&state),
                Some(super::super::host::CompletionAction::PostMutationObservation),
                "the mutation epoch remains open after an unproven timed pipeline: {command}"
            );
        }
    }

    #[test]
    fn workspace_validation_accepts_bound_cwd_and_nested_tmp_workspace() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/out.txt"),
            bash_record(
                1,
                "cargo test --manifest-path=/workspace/subdir/Cargo.toml",
                true,
            ),
        ];
        state.stall.turn_sigs = vec![turn_sig("write"), turn_sig("bound-equals-validator")];
        assert!(recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/out.txt"),
            bash_record(1, "cd /workspace/subdir && cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write"), turn_sig("bound-cwd-validator")];
        assert!(recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/out.txt"),
            bash_record(
                1,
                "python setup.py build_ext --inplace 2>&1 | tail -20",
                true,
            ),
        ];
        state.stall.turn_sigs = vec![turn_sig("write"), turn_sig("bound-pipeline-validator")];
        assert!(recent_activity_supports_budget_extension(&state));

        state.hooks.workspace_root_hint = Some("/tmp/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/tmp/workspace/out.txt"),
            bash_record(1, "cd /tmp/workspace/subdir && cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write"), turn_sig("nested-bound-validator")];
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_keeps_failed_canonical_validator_as_a_risk_barrier() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            bash_record(0, "cargo test", false),
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("test:workspace"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];

        assert!(!state.task_profile.mutates_workspace);
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn external_scratch_write_only_blocks_its_current_budget_window() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            write_record(0, "/tmp/scratch.txt", true),
            read_record_at_path(1, "/workspace/first.rs", 1, 20),
            read_record_at_path(2, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("scratch:write"),
            turn_sig("read:first"),
            turn_sig("read:second"),
        ];
        assert!(!recent_activity_supports_budget_extension(&state));

        // Once the scratch call is outside the current eight-record evidence
        // window, a normal read-only task may resume distinct observations.
        for round in 3..=10 {
            state.stall.tool_call_records.push(read_record_at_path(
                round,
                &format!("/workspace/read-{round}.rs"),
                1,
                20,
            ));
            state
                .stall
                .turn_sigs
                .push(turn_sig(&format!("read:{round}")));
        }
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_rejects_repetitive_read_only_loop() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![read_record(0, 1, 80), read_record(1, 1, 80)];
        state.stall.turn_sigs = vec![turn_sig("read_file:1-80"), turn_sig("read_file:1-80")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "repeating the same tool signature should not earn more budget"
        );
    }

    #[test]
    fn external_scratch_shell_proof_is_narrow_and_fail_closed() {
        let workspace = Some("/workspace");

        assert!(bash_mutation_is_proven_external_scratch(
            "printf x >/tmp/scratch.txt",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "echo x > '/var/tmp/scratch file'",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && git show HEAD:src/lib.rs > /tmp/inspected.rs && sed -n '1,20p' /tmp/inspected.rs",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && git diff 449b13b95f56f57619094fbb8afbc496d31dd7a8^ 449b13b95f56f57619094fbb8afbc496d31dd7a8 > /tmp/review.diff && wc -l /tmp/review.diff",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel)\"; git show HEAD:src/lib.rs > /tmp/inspected.rs 2>&1; wc -l /tmp/inspected.rs",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "mkdir -p /tmp/review && git show HEAD:src/lib.rs > /tmp/review/inspected.rs 2>&1 && wc -l /tmp/review/inspected.rs",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && for f in crates/runtime/src/turn/agentic_loop/host.rs crates/runtime/src/turn/agentic_loop/lifecycle.rs; do echo \"######## $f ########\"; git show HEAD -- \"$f\" | tail -n +120; done > /tmp/loopdiff.txt 2>&1; wc -l /tmp/loopdiff.txt",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel)\"; git show HEAD:src/lib.rs > /tmp/inspected.rs; grep -n foo /tmp/inspected.rs",
            workspace,
        ));
        assert!(bash_mutation_is_proven_external_scratch(
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null)\"; git --no-pager diff 449b13b95^ 449b13b95 -- crates/services/src/storage.rs > /tmp/sd.diff 2>&1; wc -l /tmp/sd.diff",
            workspace,
        ));

        // Source operands, mode operands, multiple targets, and control flow
        // are not proven by the small shell recognizer.  They must remain
        // mutation barriers until the executor supplies a typed receipt.
        for command in [
            "mv /workspace/a /tmp/out",
            "mkdir -p /workspace/review",
            "chmod 644 /workspace/a /tmp/out",
            "sed -i s/a/b/ /workspace/a /tmp/out",
            "printf x >/tmp/scratch.txt >/workspace/out.txt",
            "printf x >/tmp/scratch.txt 2>/workspace/error.txt",
            "printf x >/workspace/out.txt >/tmp/scratch.txt",
            "printf x >/tmp/scratch.txt && unknown-writer",
            "unknown-writer >/tmp/scratch.txt",
            "git show HEAD:src/lib.rs >/workspace/out.txt",
            "printf x >\"$TMPDIR/out\"",
        ] {
            assert!(
                !bash_mutation_is_proven_external_scratch(command, workspace),
                "unexpected scratch proof for {command:?}"
            );
        }

        // A sequence is safe when every stage is independently proven and
        // every writer target is external; it is not safe merely because the
        // first writer targets /tmp.
        assert!(bash_mutation_is_proven_external_scratch(
            "printf x >/tmp/scratch.txt; true",
            workspace,
        ));
        assert!(!bash_mutation_is_proven_external_scratch(
            "for f in /workspace/a; do rm -f /workspace/a; done >/tmp/scratch.txt",
            workspace,
        ));

        // A bound workspace nested below /tmp is still a real workspace, not
        // disposable scratch merely because of its filesystem prefix.
        assert!(!bash_mutation_is_proven_external_scratch(
            "printf x >/tmp/workspace/out.txt",
            Some("/tmp/workspace"),
        ));
    }

    #[test]
    fn budget_extension_rejects_different_signatures_with_same_observation() {
        let mut state = make_state();
        for round in 0..8 {
            let mut record = read_record(round, round * 10, round * 10 + 9);
            record.result_full = Some("unchanged evidence".into());
            state.stall.tool_call_records.push(record);
            state
                .stall
                .turn_sigs
                .push(turn_sig(&format!("read_file:range-{round}")));
        }

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "argument/range spelling alone must not renew a slice when the observed receipt is unchanged"
        );
    }

    #[test]
    fn mutating_turn_does_not_renew_from_distinct_workspace_observations() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 20),
            read_record_at_path(1, "/workspace/second.rs", 1, 20),
        ];
        state.stall.turn_sigs = vec![turn_sig("read:first"), turn_sig("read:second")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "a MustMutate turn needs typed task-facing progress, not new reads"
        );

        state.task_profile.mutates_workspace = false;
        assert!(
            recent_activity_supports_budget_extension(&state),
            "read-only investigation retains distinct-evidence renewal"
        );
    }

    #[test]
    fn budget_extension_accepts_a_successful_recovery_of_an_exact_failed_operation() {
        let mut state = make_state();
        let mut failed = read_record(0, 1, 20);
        failed.ok = false;
        failed.error = Some("transient read failure".into());
        let recovered = read_record(1, 1, 20);
        state.stall.tool_call_records = vec![failed, recovered];
        state.stall.turn_sigs = vec![turn_sig("read_file:failed"), turn_sig("read_file:retry")];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "a same-operation success resolving a prior typed failure is evidence delta"
        );
    }

    #[test]
    fn budget_extension_rejects_all_failed_activity() {
        let mut state = make_state();
        let mut failed = read_record(0, 1, 80);
        failed.ok = false;
        failed.error = Some("file not found".into());
        state.stall.tool_call_records = vec![failed];
        state.stall.turn_sigs = vec![turn_sig("read_file:missing")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "failed-only activity should not extend the turn"
        );
    }

    #[test]
    fn budget_extension_rejects_arbitrary_success_after_unvalidated_mutation() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "ls -la /workspace", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:lib"), turn_sig("list:workspace")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "an unrelated successful observation must not buy a new mutating slice"
        );
    }

    #[test]
    fn budget_extension_requires_validation_after_latest_mutation() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/first.rs"),
            bash_record(1, "cargo test -p changed-package", true),
            committed_write_record(2, "/workspace/src/second.rs"),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("write:first"),
            turn_sig("test:changed-package"),
            turn_sig("write:second"),
        ];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "a successful check becomes stale when a later deliverable mutation opens a new validation epoch"
        );
    }

    #[test]
    fn budget_extension_rejects_volatile_scratch_churn() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/app".into());
        for round in 0..8 {
            let record = if round % 2 == 0 {
                write_record(round, &format!("/tmp/encoder-{round}.c"), true)
            } else {
                bash_record(round, &format!("cc /tmp/encoder-{}.c", round - 1), false)
            };
            state.stall.tool_call_records.push(record);
            state
                .stall
                .turn_sigs
                .push(turn_sig(&format!("scratch:{round}")));
        }

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "distinct /tmp candidates plus distinct failed checks are activity, not stable task progress"
        );
    }

    #[test]
    fn budget_extension_rejects_scratch_mutation_even_after_unrelated_test() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/app".into());
        state.stall.tool_call_records = vec![
            write_record(0, "/tmp/encoder.c", true),
            bash_record(1, "cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("scratch:write"), turn_sig("test:generic")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "a validator cannot turn an external scratch artifact into a task deliverable"
        );
    }

    #[test]
    fn budget_extension_rejects_bash_scratch_mutation_even_after_test() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/app".into());
        state.stall.tool_call_records = vec![
            bash_record(0, "printf source > /tmp/encoder.c", true),
            bash_record(1, "cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("scratch:bash"), turn_sig("test:generic")];

        assert!(
            !recent_activity_supports_budget_extension(&state),
            "a shell redirect into external scratch must not masquerade as a deliverable mutation"
        );
    }

    #[test]
    fn budget_extension_requires_validation_after_same_bash_mutation() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![bash_record(
            0,
            "cargo test && sed -i 's/a/b/' /workspace/src/lib.rs",
            true,
        )];
        state.stall.turn_sigs = vec![turn_sig("test-then-write")];
        assert!(!recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = authoritative_bash_mutation_record(
            0,
            "sed -i 's/a/b/' /workspace/src/lib.rs && cargo test",
        );
        state.stall.turn_sigs[0] = turn_sig("write-then-test");
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_requires_validator_after_the_final_mutation_barrier() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        let command =
            "cargo test && sed -i 's/a/b/' /workspace/src/lib.rs && test -e /workspace/src/lib.rs";
        state.stall.tool_call_records = vec![bash_record(0, command, true)];
        state.stall.turn_sigs = vec![turn_sig("stale-validator")];
        assert!(!recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = authoritative_bash_mutation_record(
            0,
            "sed -i 's/a/b/' /workspace/src/lib.rs && cargo test",
        );
        state.stall.turn_sigs[0] = turn_sig("post-validator");
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_accepts_local_validation_in_the_same_bash_record() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![authoritative_bash_mutation_record(
            0,
            "printf x > '/workspace/out file' && cmp /workspace/expected '/workspace/out file'",
        )];
        state.stall.turn_sigs = vec![turn_sig("local-post-validation")];
        assert!(recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = bash_record(
            0,
            "cmp /workspace/expected '/workspace/out file' && printf x > '/workspace/out file'",
            true,
        );
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_scopes_local_receipt_to_its_final_operands() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());

        state.stall.tool_call_records = vec![authoritative_bash_mutation_record(
            0,
            "printf bad > /workspace/out && cmp /workspace/foo /workspace/bar",
        )];
        state.stall.turn_sigs = vec![turn_sig("unrelated-local-cmp")];
        assert!(!recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = authoritative_bash_mutation_record(
            0,
            "printf good > /workspace/out && cmp /workspace/expected /workspace/out",
        );
        assert!(recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = authoritative_bash_mutation_record(
            0,
            "printf good > /workspace/out && cmp /workspace/expected /workspace/out || true",
        );
        assert!(!recent_activity_supports_budget_extension(&state));

        state.stall.tool_call_records[0] = authoritative_bash_mutation_record(
            0,
            "printf good > /workspace/out && cmp /workspace/expected /workspace/out;",
        );
        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_rejects_validation_before_an_unknown_writer() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![bash_record(
            0,
            "cargo test && python3 -c \"open('/workspace/src/lib.rs','w').write('bad')\"",
            true,
        )];
        state.stall.turn_sigs = vec![turn_sig("test-then-unknown-write")];
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_closes_validation_epoch_after_failed_partial_mutation() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "cargo test", true),
            bash_record(2, "printf bad > /workspace/src/lib.rs && false", false),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("write:lib"),
            turn_sig("test:lib"),
            turn_sig("partial-write:lib"),
        ];
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_rejects_preview_only_or_unscoped_mutations() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            args_preview: Some(r#"{"path":"/workspace/src/lib.rs"}"#.into()),
            round: Some(0),
            ..Default::default()
        }];
        state.stall.turn_sigs = vec![turn_sig("preview-only")];
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn bash_preview_never_counts_as_historical_mutation_evidence() {
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_preview: Some(r#"{"command":"printf x > /workspace/out"}"#.into()),
            ..Default::default()
        };
        assert!(!tool_record_is_workspace_mutation(&record));
    }

    #[test]
    fn budget_extension_scopes_shell_targets_not_inputs_or_parent_escape() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/tmp/project".into());
        for (round, command) in [
            (0, "cat /app/input > /tmp/candidate"),
            (1, "printf '/app/literal' > /tmp/candidate"),
            (2, "cp /app/input /tmp/candidate"),
        ] {
            let record = bash_record(round, command, true);
            assert!(!record_is_stable_workspace_mutation(&state, &record));
        }
        let escaped = write_record(0, "/tmp/project/../candidate", true);
        assert!(!record_is_stable_workspace_mutation(&state, &escaped));
        let relative = write_record(2, "../tmp/out", true);
        assert!(!record_is_stable_workspace_mutation(&state, &relative));
        let in_workspace = committed_write_record(1, "/tmp/project/src/lib.rs");
        assert!(record_is_stable_workspace_mutation(&state, &in_workspace));

        for command in [
            "printf x > \"/tmp/candidate\"",
            "printf x > \"$TMPDIR/candidate\"",
            "cd /tmp && touch candidate",
            "cd .. && touch candidate",
            "cp -t /tmp source",
            "mv -t /tmp source",
            "cp src '/tmp/volatile file'",
            "mv src '/tmp/volatile file'",
            "sed -i s/x/y/ '/tmp/volatile file'",
        ] {
            assert!(!record_is_stable_workspace_mutation(
                &state,
                &bash_record(3, command, true)
            ));
        }

        assert!(record_is_stable_workspace_mutation(
            &state,
            &authoritative_bash_mutation_record(5, "sed -i 's/$/x/' '/tmp/project/src/lib.rs'")
        ));

        let quoted = committed_write_record(4, "/tmp/project/one file");
        assert!(record_is_stable_workspace_mutation(&state, &quoted));
    }

    #[test]
    fn foreground_scope_receipt_completes_current_mutation_but_cannot_renew_budget() {
        let state = make_state();
        let mut record = bash_record(0, "./opaque-writer", true);
        let receipt = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        record.workspace_mutation_observed = Some(true);
        record.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        record.workspace_mutation_receipt = receipt
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();

        assert!(tool_record_is_workspace_mutation(&record));
        assert!(!crate::turn::agentic_loop::execution_phase::record_has_trusted_workspace_mutation_receipt(
            &record
        ));
        assert!(!record_is_stable_workspace_mutation(&state, &record));
    }

    #[test]
    fn workspace_observation_shape_preserves_shell_status_boundaries() {
        assert!(!bash_command_has_workspace_observation_shape("echo ok"));
        for command in [
            "git --version status",
            "cat --version",
            "head -n 0",
            "sed -n",
            "stat --version",
            "test foo",
            "[ foo ]",
            "cargo test --help",
            "pytest --help",
            "touch /workspace/out | cargo test",
            "cargo test | touch /workspace/out",
            "cat /workspace/source > /workspace/output",
        ] {
            assert!(
                !bash_command_has_workspace_observation_shape(command),
                "metadata/option-only reader must not be a workspace receipt: {command}"
            );
        }
        assert!(bash_command_has_workspace_observation_shape("git status"));
        assert!(bash_command_has_workspace_observation_shape(
            "cargo test -v"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "cat /workspace/result"
        ));
        assert!(bash_command_has_workspace_observation_shape("ls"));
        assert!(bash_command_has_workspace_observation_shape(
            "test -e /workspace/result"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "[ -f /workspace/result ]"
        ));
        for command in [
            "grep -n pattern /workspace/result",
            "grep -e pattern /workspace/result",
            "grep -h pattern /workspace/result",
            "grep -- -h /workspace/result",
            "grep '>' /workspace/result",
            "wc -c /workspace/result",
            "ls -h",
        ] {
            assert!(
                bash_command_has_workspace_observation_shape(command),
                "valid reader must retain its workspace operand: {command}"
            );
        }
        assert!(!bash_command_has_workspace_observation_shape(
            "cat /workspace/missing || true"
        ));
        assert!(!bash_command_has_workspace_observation_shape(
            "cat /workspace/missing; echo ok"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "cat /workspace/result && true"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "cat /workspace/missing || cat /workspace/result"
        ));
        assert!(!bash_command_has_workspace_observation_shape(
            "cargo test || true"
        ));
        assert!(!bash_command_has_workspace_observation_shape(
            "cargo test; true"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "cargo test && true"
        ));
        for command in [
            "printf x | cat",
            "[ foo ] | cat",
            "printf x | sha256sum",
            "test foo | cat",
            "test foo | true",
        ] {
            assert!(
                !bash_command_has_workspace_observation_shape(command),
                "a stdin-only downstream reader cannot manufacture workspace evidence: {command}"
            );
        }
        for command in ["cargo test | tail -20", "cat /workspace/result | head"] {
            assert!(
                bash_command_has_workspace_observation_shape(command),
                "a downstream reader may inherit evidence from a workspace-producing stage: {command}"
            );
        }
        assert!(bash_command_has_workspace_observation_shape(
            "cat /workspace/source > /workspace/output && cat /workspace/output"
        ));
        assert!(bash_command_has_workspace_observation_shape(
            "[ -f /workspace/result ] | cat"
        ));
    }

    #[test]
    fn scoped_workspace_observation_uses_the_ordered_receipt_segment() {
        let root = Some("/tmp/project");
        for command in [
            "cargo test > /tmp/test.log 2>&1; rm /tmp/test.log; git status --short",
            "cat /tmp/external-input; git status --short",
            "rm /tmp/scratch && cd /tmp/project && cargo test -q",
        ] {
            assert!(
                bash_command_has_scoped_workspace_observation(command, root),
                "a final bound-workspace receipt must survive earlier external scratch work: {command}"
            );
        }

        for command in [
            "git status --short; touch /tmp/later",
            "git status --short || touch /tmp/later",
            "cd /tmp && git status --short",
            "cat /tmp/unrelated",
        ] {
            assert!(
                !bash_command_has_scoped_workspace_observation(command, root),
                "external/stale evidence must not satisfy the bound-workspace contract: {command}"
            );
        }
    }

    #[test]
    fn successful_bound_script_execution_is_a_workspace_observation() {
        let root = Some("/app");

        for command in [
            "cd /app && python3 solution.py",
            "cd /app && node verify.mjs",
            "cd /app && bash acceptance.sh",
        ] {
            assert!(
                bash_command_has_scoped_workspace_observation(command, root),
                "executing a literal script from the bound workspace observes the delivered artifact: {command}"
            );
        }
        for command in [
            "cd /app && python3 -c 'print(1)'",
            "cd /app && python3 $SCRIPT",
            "cd /app && python3 /tmp/attack.py",
            "cd /app && python3 verify.js",
            "cd /app && node verify.py",
            "cd /app && python3 solution.py | cat",
            "cd /app && python3 solution.py | head -1",
            "cd /app && touch earlier.txt && python3 solution.py",
            "cd /app && python3 solution.py && touch later.txt",
            "cd /app && python3 solution.py && rm -f obsolete.txt",
            "cd /app && python3 solution.py || true",
            "cd /app && python3 solution.py; true",
            "cd /app && python3 solution.py > result.txt",
        ] {
            assert!(
                !bash_command_has_scoped_workspace_observation(command, root),
                "inline, dynamic, or external interpreter input is not bound-workspace evidence: {command}"
            );
        }
        assert_eq!(
            bash_literal_script_artifact_observation_target(
                "cd /app && python3 solution.py | cat",
                root,
            ),
            None,
            "a pipeline cannot prove that the delivered script ran to completion"
        );
        assert_eq!(
            bash_literal_script_artifact_observation_target(
                "cd /app && touch earlier.txt && python3 solution.py",
                root,
            ),
            None,
            "an unrelated mutation in the invocation prevents exact artifact attribution"
        );
    }

    #[test]
    fn budget_extension_keeps_workspace_nested_under_tmp_stable() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/tmp/project".into());
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/tmp/project/src/lib.rs"),
            bash_record(1, "cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:tmp-workspace"), turn_sig("test:project")];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "a bound workspace may itself live under /tmp; only paths outside that root are volatile scratch"
        );
    }

    #[test]
    fn budget_extension_accepts_completed_explicit_acceptance_contract() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "./quality-gate", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:lib"), turn_sig("hook:quality")];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "caller-authored acceptance coverage is typed progress even when its command family is project-specific"
        );
    }

    #[test]
    fn budget_extension_rejects_mutation_after_explicit_acceptance() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "./quality-gate", true),
            bash_record(2, "printf bad > /workspace/src/lib.rs && false", false),
        ];
        state.stall.turn_sigs = vec![
            turn_sig("write:lib"),
            turn_sig("hook:quality"),
            turn_sig("partial-write:lib"),
        ];
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_allows_one_probationary_multi_file_slice() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 6,
            extension_turns: 2,
            max_extensions: 2,
            renewable_past_review_limit: true,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            committed_write_record(1, "/workspace/src/main.rs"),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:lib"), turn_sig("write:main")];
        assert!(recent_activity_supports_budget_extension(&state));
        assert!(maybe_extend_turn_budget(&mut state).is_some());

        state.max_turns = 4;
        state.remaining_turns = 0;
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_does_not_count_chmod_mode_as_a_second_target() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            bash_record(0, "chmod 644 /workspace/out.txt", true),
            read_record_at_path(1, "/workspace/out.txt", 1, 20),
        ];
        state.stall.turn_sigs = vec![turn_sig("chmod:out"), turn_sig("read:out")];
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_does_not_reuse_receipt_from_a_previous_slice() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 8,
            extension_turns: 2,
            max_extensions: 3,
            renewable_past_review_limit: true,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "cargo test", true),
        ];
        state.stall.turn_sigs = vec![turn_sig("write:lib"), turn_sig("test:lib")];
        assert!(maybe_extend_turn_budget(&mut state).is_some());

        // The old receipt remains in the journal, but no fresh mutation or
        // validation occurred in the newly granted slice.
        state
            .stall
            .tool_call_records
            .push(read_record_at_path(2, "/workspace/other.rs", 1, 20));
        state.stall.turn_sigs.push(turn_sig("read:other"));
        assert!(!recent_activity_supports_budget_extension(&state));
    }

    #[test]
    fn budget_extension_rejects_observation_only_file_and_self_comparison() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        for command in [
            "printf x > /workspace/out.txt && file /workspace/out.txt",
            "printf x > /workspace/out.txt && cmp /workspace/out.txt /workspace/out.txt",
        ] {
            state.stall.tool_call_records = vec![bash_record(0, command, true)];
            state.stall.turn_sigs = vec![turn_sig(command)];
            assert!(!recent_activity_supports_budget_extension(&state));
        }
    }

    #[test]
    fn budget_extension_does_not_treat_policy_convergence_as_a_hard_stop() {
        use astra_turn_core::context_feedback::{
            RuntimePolicyFeedbackEntry, RuntimePolicyFeedbackSet, RuntimePolicyRecommendation,
            RuntimePolicySignal, RuntimePolicyStage, RuntimePolicySubject,
        };

        let mut state = make_state();
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 80),
            read_record_at_path(1, "/workspace/second.rs", 81, 160),
        ];
        state.stall.turn_sigs = vec![turn_sig("read_file:first"), turn_sig("read_file:second")];
        state.stall.active_policy_feedback = RuntimePolicyFeedbackSet::Evaluated {
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 2,
            evaluated_at_round: 12,
            subject: RuntimePolicySubject::Run,
            entries: vec![RuntimePolicyFeedbackEntry {
                signal: RuntimePolicySignal::LowYieldRoundChurn,
                stage: RuntimePolicyStage::Converge,
                observed_at_round: 12,
                evidence_count: 12,
                recommendation: RuntimePolicyRecommendation::SynthesizeAndDecide,
            }],
        };

        assert!(recent_activity_supports_budget_extension(&state));
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
            renewable_past_review_limit: false,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        assert!(
            maybe_extend_turn_budget(&mut state).is_some(),
            "policy feedback is an alert; concrete recent progress still earns a bounded slice"
        );
        assert!(!state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn warning_verdict_does_not_revoke_productive_adaptive_slice() {
        let mut state = make_state();
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
            renewable_past_review_limit: false,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 80),
            read_record_at_path(1, "/workspace/second.rs", 1, 80),
        ];
        state.stall.turn_sigs = vec![turn_sig("read:first"), turn_sig("read:second")];
        state.stall.verdict_events.push(
            astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "warning".into(),
                injections: vec!["narrow the search".into()],
                avoid_tools: vec![],
                health_avoidance_tools: vec![],
                advisory_threshold_reached: true,
                nudge_count: 1,
                interaction_mode: "prompt".into(),
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
                total_errors: 0,
                health_avoidance_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        );

        assert!(maybe_extend_turn_budget(&mut state).is_some());
        assert!(!state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn critical_verdict_revokes_adaptive_checkpoint_guidance() {
        let mut state = make_state();
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
            renewable_past_review_limit: false,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.verdict_events.push(
            astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "critical".into(),
                injections: vec!["settle from evidence".into()],
                avoid_tools: vec![],
                health_avoidance_tools: vec![],
                advisory_threshold_reached: true,
                nudge_count: 1,
                interaction_mode: "prompt".into(),
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
                total_errors: 0,
                health_avoidance_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        );

        assert!(!adaptive_budget_is_renewable(&state));
        assert!(
            crate::prompts::execution_slice_guidance(
                state.remaining_turns,
                state.max_turns,
                adaptive_budget_is_renewable(&state),
            )
            .contains("Do not call any tool"),
            "a critical verdict must keep the terminal guidance aligned with the scheduler"
        );
    }

    #[test]
    fn distinct_typed_read_operations_extend_even_when_path_is_reused() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        let mut records = Vec::new();
        let mut signatures = Vec::new();
        for round in 0..10 {
            records.push(read_record_at_path(
                round,
                "/workspace/src/lib.rs",
                round * 10 + 1,
                round * 10 + 10,
            ));
            signatures.push(turn_sig(&format!("read:lib:{round}")));
        }
        state.stall.tool_call_records = records;
        state.stall.turn_sigs = signatures;

        assert!(recent_activity_supports_budget_extension(&state));
    }

    #[tokio::test]
    async fn profile_budget_extension_uses_recent_activity_not_stale_side_flags() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
            renewable_past_review_limit: true,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![
            read_record_at_path(0, "/workspace/first.rs", 1, 80),
            read_record_at_path(1, "/workspace/second.rs", 81, 160),
        ];
        state.stall.turn_sigs = vec![turn_sig("read_file:first"), turn_sig("read_file:second")];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 2)
            .await
            .expect("prepare should continue after extension");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "recent useful activity should drive extension at budget exhaustion"
        );
        assert_eq!(state.max_turns, 4);
        assert_eq!(
            state.remaining_turns, 1,
            "extension adds two turns, then current prepare consumes one"
        );
        assert!(
            state.volatile_pending.iter().all(|entry| {
                !entry.payload.to_string().contains("remaining_turns")
                    && !entry.payload.to_string().contains("turn_budget")
            }),
            "non-terminal budget facts must remain runtime-internal"
        );
    }

    #[test]
    fn validated_sequential_tool_rounds_can_extend_the_budget() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 9,
            hard_turn_limit: 13,
            extension_turns: 4,
            max_extensions: 1,
            renewable_past_review_limit: true,
        };
        state.max_turns = 9;
        state.remaining_turns = 0;
        for round in 0..9 {
            state.stall.tool_call_records.push(committed_write_record(
                round,
                &format!("/workspace/out/{round}.txt"),
            ));
            state
                .stall
                .turn_sigs
                .push(turn_sig(&format!("write_file:out/{round}.txt")));
            state
                .messages
                .push(json!({"role": "assistant", "tool_calls": []}));
            state
                .messages
                .push(json!({"role": "tool", "content": "written"}));
        }
        state
            .stall
            .tool_call_records
            .push(bash_record(9, "cargo test -p changed-package", true));
        state.stall.turn_sigs.push(turn_sig("test:changed-package"));

        assert_eq!(
            crate::prompts::trailing_single_tool_round_streak(&state.messages),
            9
        );
        assert!(recent_activity_supports_budget_extension(&state));
        assert!(compute_stall_diagnosis(&state).signal.is_none());
        assert!(maybe_extend_turn_budget(&mut state).is_some());
        assert_eq!(state.max_turns, 13);
        assert_eq!(state.remaining_turns, 4);
    }

    #[tokio::test]
    async fn hard_agentic_limit_reserves_a_text_only_answer_boundary() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::ReadOnly));
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 2,
            extension_turns: 0,
            max_extensions: 0,
            renewable_past_review_limit: false,
        };
        state.max_turns = 2;
        state.remaining_turns = 1;
        state.stall.tool_call_records = vec![read_record(0, 1, 80)];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .expect("the final evidence-bearing round must remain answerable");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.max_turns, 3);
        assert_eq!(state.remaining_turns, 1);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.budget_wrapup_injected);
        assert!(state.interruption.is_none());
        let settlement = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "agentic_execution_slice_complete")
            .expect("typed settlement instruction");
        assert_eq!(settlement.payload["evidence"]["tool_calls_completed"], 1);
        assert!(
            !settlement.payload.to_string().contains("hard_turn_limit"),
            "internal counters must not leak into model-facing product copy"
        );
    }

    #[test]
    fn hard_boundary_does_not_reopen_execution_for_uncertain_generic_action() {
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MayMutate));
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![read_record(0, 1, 80)];

        assert!(begin_budget_settlement(&mut state));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "a generic action is not an exact completion obligation"
        );
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.budget_wrapup_injected);
        let payload = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "agentic_execution_slice_complete")
            .expect("text-only settlement frame");
        assert_eq!(payload.payload["execution_authority"], "none");
    }

    #[test]
    fn hard_boundary_reserves_one_typed_completion_action_then_text() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.max_turns = 2;
        state.remaining_turns = 0;
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));

        assert!(begin_budget_settlement(&mut state));
        assert_eq!(state.max_turns, 4);
        assert_eq!(state.remaining_turns, 2);
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("typed settlement must expose one action");
        assert_eq!(
            window.action,
            super::super::host::CompletionAction::PostMutationObservation
        );
        assert_eq!(window.attempts_remaining, 1);
        let payload = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "typed_completion_action_available")
            .expect("typed action frame");
        assert_eq!(payload.payload["schema"], "completion_settlement.v2");
        assert_eq!(
            payload.payload["execution_authority"],
            "one_matching_action"
        );
    }

    #[test]
    fn active_work_boundary_verifies_latest_mutation_before_typed_settlement() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.max_turns = 50;
        state.remaining_turns = 0;
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));

        assert!(begin_budget_settlement_for_work_state(&mut state, true));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect(
                "the latest mutation must receive one typed observation before Work settlement",
            );
        assert_eq!(
            window.action,
            super::super::host::CompletionAction::PostMutationObservation
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert_eq!(state.remaining_turns, 2);
    }

    #[test]
    fn active_work_boundary_reserves_one_canonical_revalidation_then_settlement() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.max_turns = 32;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                result_full: Some(
                    serde_json::json!({
                        "status": "assigned",
                        "execution": "primary_session",
                        "attempt_id": "attempt-a"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(r#"{"command":"cargo test"}"#.into()),
                runtime_args_full: Some(r#"{"command":"cargo test"}"#.into()),
                ..Default::default()
            },
            committed_write_record(31, "/workspace/src/lib.rs"),
        ];

        assert!(begin_budget_settlement_for_work_state(&mut state, true));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("stale Work validation must receive one bounded revalidation action");
        assert_eq!(
            window.action,
            super::super::host::CompletionAction::CanonicalWorkValidation
        );
        assert_eq!(state.max_turns, 34);
        assert_eq!(state.remaining_turns, 2);
        assert!(!state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn external_effect_without_work_validation_debt_does_not_gain_revalidation_authority() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"package-manager install artifact"}"#.into()),
            runtime_args_full: Some(r#"{"command":"package-manager install artifact"}"#.into()),
            ..Default::default()
        }];

        assert_ne!(
            super::super::execution_phase::pending_completion_action_for_work_state(&state, true),
            Some(super::super::host::CompletionAction::CanonicalWorkValidation)
        );
    }

    #[test]
    fn active_work_dependency_chain_cannot_skip_to_work_settlement() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        let first = explicit_verification_hook("compile", "./compile-gate");
        let mut second = explicit_verification_hook("acceptance", "./acceptance-gate");
        second.depends_on = vec!["compile".into()];
        state.hooks.stop_hooks = vec![first, second];
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            read_record_at_path(1, "/workspace/src/lib.rs", 1, 20),
        ];
        state.max_turns = 50;
        state.remaining_turns = 0;

        let action = super::super::execution_phase::pending_completion_action(&state)
            .expect("both dependent verification hooks remain pending");
        assert!(matches!(
            action,
            super::super::host::CompletionAction::ExplicitVerification { .. }
        ));
        assert!(
            !super::super::execution_phase::completion_action_window_is_batchable(&state, &action)
        );
        assert!(!begin_budget_settlement_for_work_state(&mut state, true));
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[tokio::test]
    async fn ignored_work_settlement_retries_once_then_fails_without_pausing() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 6,
            extension_turns: 2,
            max_extensions: 2,
            renewable_past_review_limit: true,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.hooks.completion_settlement.work_settlement_only = true;
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            bash_record(1, "cargo test -p changed-package", true),
            ToolCallRecord {
                name: "settle_work_item".into(),
                ok: false,
                error: Some("transient settlement failure".into()),
                round: Some(2),
                ..Default::default()
            },
        ];
        state.stall.turn_sigs = vec![
            turn_sig("write_file:lib"),
            turn_sig("test:changed-package"),
            turn_sig("settle_work_item:failed"),
        ];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "the counterexample must carry enough stale progress to renew an ordinary exploratory slice"
        );

        let retry = prepare_turn_iteration(&mut host, &mut state, 2)
            .await
            .expect("first ignored settlement receives one focused retry");
        assert!(matches!(retry, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.budget_wrapup_ignored_rounds, 1);
        assert_eq!(
            state.max_turns, 3,
            "settlement may add only its single retry, never an exploratory extension"
        );
        assert_eq!(state.remaining_turns, 0);
        assert!(!state.hooks.completion_settlement.text_only);

        let terminal = prepare_turn_iteration(&mut host, &mut state, 3)
            .await
            .expect("repeated contract failure converges deterministically");
        assert!(matches!(
            terminal,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.final_text,
            super::super::host::WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT
        );
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete),
            "an invalid settlement response must be exposed as an incomplete run"
        );
    }

    #[tokio::test]
    async fn unused_extension_headroom_is_not_stolen_by_settlement_reserve() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
            renewable_past_review_limit: true,
        };
        state.max_turns = 2;
        state.remaining_turns = 1;
        state.stall.tool_call_records = vec![read_record(0, 1, 80), read_record(1, 81, 160)];
        state.stall.turn_sigs = vec![turn_sig("read_file:1-80"), turn_sig("read_file:81-160")];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .expect("adaptive headroom should remain available");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.max_turns, 2);
        assert_eq!(state.remaining_turns, 0);
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);
    }

    struct AutoRouteResolver;

    impl SkillResolver for AutoRouteResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
            Ok(ResolvedSkill {
                name: name.to_string(),
                instructions: format!("Use the {name} workflow."),
                max_tokens: None,
                allowed_tools: vec!["read_file".into()],
                execution_context: ExecutionContext::Inline,
                hooks: SkillHooks::default(),
                skill_dir: None,
                source: SkillSourceKind::Local,
                success_criteria: Vec::new(),
                composition: None,
                input_schema: None,
                output_schema: None,
                remote_url: None,
                forward_headers: Vec::new(),
                required_headers: Vec::new(),
                aliases: vec!["review changes".into()],
                effort: None,
                agent_type: None,
                trust_tier: TrustTier::Bundled,
            })
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            vec![
                SkillToolInfo {
                    name: "review-changes".into(),
                    description: "Review the current branch diff.".into(),
                    aliases: vec!["review changes".into()],
                    ..Default::default()
                },
                SkillToolInfo {
                    name: "optimize-prompt".into(),
                    description: "Reduce prompt size.".into(),
                    aliases: vec!["prompt optimization".into()],
                    ..Default::default()
                },
            ]
        }
    }

    struct FailingAutoRouteResolver;

    impl SkillResolver for FailingAutoRouteResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
            Err(crate::skills::SkillError::LoadFailed(format!(
                "cannot load {name}"
            )))
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            vec![SkillToolInfo {
                name: "review-changes".into(),
                description: "Review the current branch diff.".into(),
                aliases: vec!["review changes".into()],
                ..Default::default()
            }]
        }
    }

    #[test]
    fn interruption_state_summary_prefers_exploration_family_stall_signal() {
        let mut state = make_state();
        state.stall.tool_call_records = (0..5)
            .flat_map(|round| [read_record(round, 10, 40), read_record(round, 50, 80)])
            .collect();

        let summary = interruption_state_summary(&state, None);
        assert_eq!(
            summary.stall_signal.as_deref(),
            Some("exploration_family=read;streak=5")
        );
        assert_eq!(
            summary.resume_restricted_tools,
            Vec::<String>::new(),
            "read-heavy stall must preserve guidance without hard-blocking read tools"
        );
        let diagnosis = interruption_diagnosis_summary(&state).expect("diagnosis");
        assert!(
            diagnosis.contains("5 consecutive read-dominant exploratory rounds"),
            "expected read-family diagnosis, got {diagnosis}"
        );
        assert!(
            diagnosis.contains("redundant overlapping reads"),
            "expected redundant-read detail, got {diagnosis}"
        );
    }

    #[test]
    fn productive_single_tool_rounds_are_not_a_stall_boundary() {
        let mut state = make_state();
        state.stall.tool_call_records = (0..3)
            .map(|round| ToolCallRecord {
                name: "bash".into(),
                ok: true,
                round: Some(round),
                ..Default::default()
            })
            .collect();
        state.messages = vec![
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
        ];

        let summary = interruption_state_summary(&state, None);
        assert_eq!(summary.stall_signal, None);
        assert_eq!(
            summary.resume_restricted_tools,
            Vec::<String>::new(),
            "batching shape must not hard-block the executor needed to finish"
        );
    }

    #[test]
    fn session_turn_number_prefers_explicit_outer_turn_over_agentic_step() {
        let mut state = make_state();
        state.session_turn = 1;
        state.max_turns = 50;
        state.remaining_turns = 0;

        assert_eq!(current_agentic_step(&state), 50);
        assert_eq!(session_turn_number(&state), 1);
    }

    #[test]
    fn current_agentic_step_uses_actual_llm_rounds_when_available() {
        let mut state = make_state();
        state.max_turns = 25;
        state.remaining_turns = 9;
        state.llm_rounds_completed = 14;

        assert_eq!(
            current_agentic_step(&state),
            14,
            "agentic_step must not skip when control-only loop iterations consume remaining_turns"
        );
    }

    #[test]
    fn budget_exhaustion_summary_prefers_partial_parallel_agent_results() {
        let mut state = make_state();
        state.max_turns = 13;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 13;
        state.total_tool_calls = 8;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review bugs"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b",
                    "description":"Review bugs"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-a",
                    "result":"Architecture review finished: layout looks good."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-b"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-b",
                    "result":"Bug review finished: no blocker found."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"still_running",
                    "agent_id":"agent-c",
                    "current_status":"Running { activity: \"executing\" }"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                None,
                Some("duplicate_within_turn"),
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                None,
                Some("blocked_tool: Tool 'agent' is currently restricted in this session."),
            ),
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());

        assert!(
            text.contains("owner turn reached its execution boundary"),
            "{text}"
        );
        assert!(text.contains("Review architecture"), "{text}");
        assert!(text.contains("Architecture review finished"), "{text}");
        assert!(text.contains("Review bugs"), "{text}");
        assert!(text.contains("Bug review finished"), "{text}");
        assert!(text.contains("Review security"), "{text}");
        assert!(
            text.contains("still running") || text.contains("did not finish"),
            "{text}"
        );
        assert!(
            text.contains("duplicate_within_turn") || text.contains("restricted"),
            "{text}"
        );
    }

    #[test]
    fn budget_exhaustion_summary_uses_shared_child_result_projection() {
        let mut state = make_state();
        state.max_turns = 9;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 9;
        state.total_tool_calls = 6;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review infra"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review infra"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"interrupted",
                    "agent_id":"agent-a",
                    "finish_reason":"budget_exhausted",
                    "result":"Partial architecture findings."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-b"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"cancelled",
                    "agent_id":"agent-c",
                    "reason":"parent cancelled this sub-agent"
                })),
                None,
            ),
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());

        assert!(text.contains("Partial architecture findings."), "{text}");
        assert!(
            text.contains("launched and has not produced a child result yet"),
            "{text}"
        );
        assert!(text.contains("parent cancelled this sub-agent"), "{text}");
    }

    #[test]
    fn budget_exhaustion_summary_uses_record_projection_when_args_preview_is_missing() {
        let mut state = make_state();
        state.max_turns = 5;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 5;
        state.total_tool_calls = 4;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "spawn",
                        "agent_id": "agent-a",
                        "description": "Review architecture"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched",
                        "description":"Review architecture"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "get_result",
                        "agent_id": "agent-a"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"interrupted",
                        "result":"Partial architecture findings.",
                        "finish_reason":"budget_exhausted"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "spawn",
                        "agent_id": "agent-b",
                        "description": "Review security"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched",
                        "description":"Review security"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "get_result",
                        "agent_id": "agent-b"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());
        assert!(text.contains("Review architecture"), "{text}");
        assert!(text.contains("Partial architecture findings."), "{text}");
        assert!(text.contains("Review security"), "{text}");
    }

    #[tokio::test]
    async fn execution_settlement_cancels_unfinished_parallel_agents_before_synthesis() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turns = 13;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 13;
        state.total_tool_calls = 8;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-a",
                    "result":"Architecture review finished."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"still_running",
                    "agent_id":"agent-c",
                    "current_status":"Running { activity: \"executing\" }"
                })),
                None,
            ),
        ];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 13)
            .await
            .expect("execution settlement should remain answerable");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(host.cancelled_agent_ids, vec!["agent-c".to_string()]);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "running child execution must be cancelled before synthesis, not consume the terminal task action"
        );
        assert!(state.interruption.is_none());
    }

    #[tokio::test]
    async fn stale_runtime_parent_does_not_sweep_reused_child_run_identity() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.execution_lease_lost = Some(Arc::new(AtomicBool::new(true)));
        state.stall.tool_call_records = vec![agent_record(
            "spawn",
            json!({"description":"new generation child"}),
            Some(json!({
                "status":"launched",
                "agent_id":"child-run-reused-by-n-plus-one",
                "description":"new generation child"
            })),
            None,
        )];

        let cancelled = cancel_unfinished_child_agents(
            &mut host,
            &state,
            "stale parent lease lost",
            CancellationOrigin::Runtime,
        )
        .await;

        assert!(cancelled.is_empty());
        assert!(
            host.cancelled_agent_ids.is_empty(),
            "a stale runtime-owned parent must not issue run-id-scoped cancellation against a newer child generation"
        );

        let cancelled = cancel_unfinished_child_agents(
            &mut host,
            &state,
            "canonical ancestor user cancellation",
            CancellationOrigin::User,
        )
        .await;
        assert!(cancelled.contains("child-run-reused-by-n-plus-one"));
        assert_eq!(
            host.cancelled_agent_ids,
            vec!["child-run-reused-by-n-plus-one".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn child_agent_cancel_timeout_does_not_block_budget_wrapup() {
        let mut host = MockHost::new(Vec::new())
            .with_cancel_child_agents_delay(CHILD_AGENT_CANCEL_TIMEOUT + Duration::from_secs(30));

        let cancelled = cancel_child_agents_with_timeout(
            &mut host,
            None,
            vec!["agent-c".to_string()],
            "owner execution boundary reached",
            CancellationOrigin::Runtime,
        )
        .await;

        assert!(
            cancelled.is_empty(),
            "a hung child-agent cancel hook should not claim cancellation"
        );
        assert!(
            host.cancelled_agent_ids.is_empty(),
            "timeout must cancel the host future before it mutates cancellation state"
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_pre_routes_judged_skill_requests() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());
        state.skills.request_constraints.allowed_tools =
            Some(std::collections::HashSet::from(["read_file".to_string()]));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");
        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.remaining_turns, 9,
            "pre-routing still consumes the current turn budget"
        );
        assert_eq!(state.tool_results.len(), 1);
        assert!(state.skills.invoked.contains_key("review-changes"));
        assert_eq!(
            state.messages.iter().find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("id")?
                    .as_str()
            }),
            Some("skill-auto-route-review-changes")
        );
        assert_eq!(
            state.messages.iter().find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("function")?
                    .get("name")?
                    .as_str()
            }),
            Some(crate::turn::skill_tool::SKILL_TOOL_NAME)
        );
        assert_eq!(
            state.telemetry.all_selected_skills,
            vec!["review-changes".to_string()]
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| content.contains("<skill-loaded name=\"review-changes\"/>"))
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| content.contains("Declared workflow tools: `read_file`")),
            "auto-routed skills must expose their typed workflow guidance at the model boundary"
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| {
                    content
                        .contains("request policy permits at most these non-skill tools: read_file")
                        && content.contains("current tool surface is authoritative")
                })
        );
        assert!(state.messages.iter().any(|msg| {
            msg.get("role").and_then(|v| v.as_str()) == Some("tool")
                && msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|content| {
                        content.contains("<skill-loaded name=\"review-changes\"/>")
                    })
        }));
    }

    #[tokio::test]
    async fn required_work_skips_optional_skill_pre_route_before_the_task_board_exists() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "verify two independent release requirements".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent =
            Some(TurnIntent::default().with_work_lifecycle(WorkLifecycleIntent::Required));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(host.skill_auto_route_queries.is_empty());
        assert!(state.skills.invoked.is_empty());
        assert!(state.tool_results.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_pre_routes_structured_user_intent_not_prompt_scaffolding() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "<project-instructions>\nMention prompt optimization here.\n</project-instructions>\n\nreview changes on current branch".into();
        state.user_intent = "review changes on current branch".into();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(state.skills.invoked.contains_key("review-changes"));
        assert_eq!(
            host.skill_auto_route_queries,
            vec!["review changes on current branch".to_string()]
        );

        let arguments = state
            .messages
            .iter()
            .find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("function")?
                    .get("arguments")?
                    .as_str()
            })
            .expect("skill auto-route tool call arguments");
        let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(parsed["task"], "review changes on current branch");
        assert!(
            !parsed["task"]
                .as_str()
                .unwrap()
                .contains("<project-instructions>")
        );
        assert!(
            !parsed["task"]
                .as_str()
                .unwrap()
                .contains("prompt optimization")
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_applies_host_judged_turn_intent() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::CodeReview)
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace)
            .with_workspace_mutation(WorkspaceMutationIntent::ReadOnly);
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "please inspect the current changes".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        assert_eq!(guard.profile.current_scenario, Some(Scenario::CodeReview));
        assert!(!state.task_profile.mutates_workspace);
        assert!(state.task_profile.exploratory_task);
        assert_eq!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("valid semantics")
                .map(|semantics| semantics.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Replace)
        );
        assert_eq!(
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation),
            Some(WorkspaceMutationIntent::ReadOnly)
        );
    }

    #[tokio::test]
    async fn unavailable_turn_intent_judge_does_not_manufacture_objective_semantics() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "repair the lifecycle".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare with the deterministic runtime profile");

        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("canonical message is valid")
                .is_none(),
            "judge unavailability is runtime state, not producer-owned objective semantics"
        );
    }

    #[tokio::test]
    async fn required_semantic_admission_never_silently_starts_primary_execution() {
        let mut host = MockHost::new(Vec::new()).with_required_turn_intent_decision();
        let mut state = make_state();
        state.message = "complete two independently verifiable outcomes".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let error = match prepare_turn_iteration(&mut host, &mut state, 0).await {
            Ok(_) => {
                panic!(
                    "a required semantic boundary must fail closed when its judge is unavailable"
                )
            }
            Err(error) => error,
        };

        assert!(error.contains("semantic task admission is temporarily unavailable"));
        assert_eq!(
            host.turn_count(),
            0,
            "the host must not call the primary model after losing the only semantic admission authority"
        );
    }

    #[tokio::test]
    async fn repeated_prepare_keeps_semantics_on_the_submitted_turn_owner() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Refine);
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
        state.session_turn = 4;
        state.message = "also verify the database path".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("first round should prepare");
        host.turn_intent = Some(
            TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace),
        );
        state.messages.push(json!({
            "role": "user",
            "content": "guidance accepted while the run is active"
        }));
        prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .expect("second round should prepare");

        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("valid semantics")
                .is_some()
        );
        assert!(
            astra_turn_types::user_turn_semantics(state.messages.last().unwrap())
                .expect("valid semantics")
                .is_none()
        );
        assert_eq!(
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Refine),
            "later model rounds must not re-judge or overwrite user-turn semantics"
        );
    }

    #[test]
    fn turn_semantics_only_advance_from_explicit_unknown_once() {
        let mut state = make_state();
        state.message = "repair it".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": "repair it"})];

        assert!(record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
        ));
        assert!(record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct)
        ));
        assert!(!record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace)
        ));
        assert_eq!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("valid semantics")
                .map(|semantics| semantics.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Correct)
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_applies_must_mutate_intent_to_runtime_profile() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::Implementation)
            .with_workspace_mutation(WorkspaceMutationIntent::MustMutate);
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(state.task_profile.mutates_workspace);
        assert!(state.task_profile.verification_required);
        assert_eq!(
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation),
            Some(WorkspaceMutationIntent::MustMutate)
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_preserves_profile_when_judge_unavailable() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "continue the implementation".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        let existing_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                true,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex,
            );
        state.task_profile = existing_profile;
        state.agentic_turn_budget = existing_profile.agentic_turn_budget;
        state.turn_guard.set_task_profile(existing_profile);
        state.turn_intent = Some(
            TurnIntent::default()
                .with_requested_scenario(Scenario::Refactoring)
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate),
        );

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.task_profile, existing_profile,
            "unavailable judge must not downgrade a current structured runtime profile"
        );
        assert_eq!(
            state.agentic_turn_budget, existing_profile.agentic_turn_budget,
            "budget must remain coupled to the preserved profile"
        );
        assert_eq!(
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation),
            Some(WorkspaceMutationIntent::MustMutate)
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_mutate_for_read_only_question_about_implementation() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::QuickAnswer)
            .with_workspace_mutation(WorkspaceMutationIntent::ReadOnly);
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
        state.message = "当前的实现，能够想起来吗？".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(!state.task_profile.mutates_workspace);
        assert!(!state.task_profile.verification_required);
    }

    #[test]
    fn auto_route_tool_call_id_is_deterministic_and_sanitized() {
        assert_eq!(
            auto_route_tool_call_id("Review Changes"),
            "skill-auto-route-review-changes"
        );
        assert_eq!(auto_route_tool_call_id("  !!!  "), "skill-auto-route");
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_pre_route_without_judge_decision() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(host.skill_auto_route_queries.is_empty());
        assert!(state.tool_results.is_empty());
        assert!(state.skills.invoked.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_rejects_judged_skill_outside_visible_catalog() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("missing-skill");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            host.skill_auto_route_queries,
            vec!["review changes on current branch".to_string()]
        );
        assert!(state.tool_results.is_empty());
        assert!(state.skills.invoked.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_retry_failed_auto_route_for_same_intent() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(FailingAutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("first turn should prepare despite failed auto-route");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.skills.auto_route_attempts.len(), 1);
        assert!(state.tool_results.is_empty());
        assert!(state.skills.invoked.is_empty());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("second turn should prepare without retrying failed auto-route");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.skills.auto_route_attempts.len(),
            1,
            "same intent+skill failure should not create repeated auto-route attempts"
        );
        assert!(state.tool_results.is_empty());
        assert!(state.skills.invoked.is_empty());
        assert_eq!(
            host.skill_auto_route_queries,
            vec![
                "review changes on current branch".to_string(),
                "review changes on current branch".to_string()
            ],
            "the semantic judge may be consulted again, but the repeated failed decision must not execute"
        );
    }

    // ── Cancellation logic tests ──────────────────────────────────────────
    //
    // P1: Lifecycle cancellation has three orthogonal dimensions:
    //   (1) cancel signal: in-memory flag vs CancellationToken
    //   (2) pause context: inside pause loop vs between turns
    //   (3) checkpoint: written on cancel path
    //
    // These tests exercise every combination of (1) and (2), plus
    // verify checkpoint and interruption record invariants.

    use crate::turn::run_control::{
        QueuedUserIntent, RunControlStatus, RunStatusProvider, UserIntentPoll, UserIntentProvider,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio_util::sync::CancellationToken;

    struct FailingStatusRunControl;

    struct CountingStatusRunControl {
        status: Option<RunControlStatus>,
        cancellation_origin: Option<CancellationOrigin>,
        subsequent_cancellation_origin: Option<CancellationOrigin>,
        calls: AtomicUsize,
        origin_calls: AtomicUsize,
    }

    impl CountingStatusRunControl {
        fn new(status: Option<RunControlStatus>) -> Self {
            Self {
                status,
                cancellation_origin: Some(CancellationOrigin::Runtime),
                subsequent_cancellation_origin: None,
                calls: AtomicUsize::new(0),
                origin_calls: AtomicUsize::new(0),
            }
        }

        fn with_cancellation_origin(mut self, origin: CancellationOrigin) -> Self {
            self.cancellation_origin = Some(origin);
            self
        }

        fn with_pending_cancellation_origin(mut self) -> Self {
            self.cancellation_origin = None;
            self
        }

        fn with_cancellation_origin_sequence(
            mut self,
            first: CancellationOrigin,
            subsequent: CancellationOrigin,
        ) -> Self {
            self.cancellation_origin = Some(first);
            self.subsequent_cancellation_origin = Some(subsequent);
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }

        fn origin_calls(&self) -> usize {
            self.origin_calls.load(Ordering::Acquire)
        }
    }

    #[async_trait::async_trait]
    impl RunStatusProvider for CountingStatusRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<RunControlStatus>, String> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(self.status)
        }

        async fn cancellation_origin(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<CancellationOrigin, String> {
            let call = self.origin_calls.fetch_add(1, Ordering::AcqRel);
            let origin = if call == 0 {
                self.cancellation_origin
            } else {
                self.subsequent_cancellation_origin
                    .or(self.cancellation_origin)
            };
            match origin {
                Some(origin) => Ok(origin),
                None => std::future::pending().await,
            }
        }
    }

    #[async_trait::async_trait]
    impl UserIntentProvider for CountingStatusRunControl {
        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: crate::turn::run_control::UserIntentAdmissionAuthority,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    #[async_trait::async_trait]
    impl RunStatusProvider for FailingStatusRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<RunControlStatus>, String> {
            Err("transient db timeout".to_string())
        }
    }

    #[async_trait::async_trait]
    impl UserIntentProvider for FailingStatusRunControl {
        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: crate::turn::run_control::UserIntentAdmissionAuthority,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    /// A bare in-memory flag has runtime origin unless durable user control
    /// proves otherwise.
    #[tokio::test]
    async fn in_memory_flag_cancellation_returns_cancelled() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));
        state.stall.tool_call_records = vec![agent_record(
            "spawn",
            json!({"description":"Review runtime"}),
            Some(json!({
                "status":"launched",
                "agent_id":"agent-running",
                "description":"Review runtime"
            })),
            None,
        )];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "flag-based cancellation must return Cancelled outcome"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
        assert_eq!(
            host.cancelled_agent_ids,
            vec!["agent-running".to_string()],
            "parent cancellation must propagate to every unfinished dynamic child"
        );
    }

    /// A bare CancellationToken is runtime-owned without durable user proof.
    #[tokio::test]
    async fn in_memory_token_cancellation_returns_cancelled() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let token = CancellationToken::new();
        token.cancel();
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "token-based cancellation must return Cancelled outcome"
        );
        assert!(state.interruption.is_none());
    }

    /// Pause flag set + cancel flag set → cancel wins inside pause loop
    /// (fast path, no DB poll needed).
    #[tokio::test]
    async fn cancel_flag_inside_pause_loop_wins_immediately() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.pause_flag = Some(Arc::new(AtomicBool::new(true)));
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation during pause should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "cancel during pause must return Cancelled immediately"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
    }

    /// Pause flag set + token cancelled → cancel wins immediately.
    #[tokio::test]
    async fn cancel_token_inside_pause_loop_wins_immediately() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let token = CancellationToken::new();
        token.cancel();
        state.cancellation.pause_flag = Some(Arc::new(AtomicBool::new(true)));
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation during pause should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "token cancel during pause must return Cancelled immediately"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
    }

    /// Clean state (no cancel, no pause) → normal turn preparation.
    #[tokio::test]
    async fn clean_state_returns_ready_for_turn_preparation() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("normal turn preparation should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "clean state without cancel/pause must return Ready"
        );
        assert!(
            state.interruption.is_none(),
            "no interruption should be recorded for clean state"
        );
        assert_eq!(
            state.remaining_turns, 9,
            "first turn should consume one budget tick"
        );
    }

    /// Neither flag nor token set → not cancelled.
    #[tokio::test]
    async fn no_cancel_signal_returns_ready() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        // flag = None, token = None → should not cancel
        state.cancellation.flag = None;
        state.cancellation.token = None;

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "None flag and None token must not trigger cancellation"
        );
    }

    /// Flag=false and token not cancelled → not cancelled.
    #[tokio::test]
    async fn flag_false_and_token_not_cancelled_returns_ready() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(false)));
        let token = CancellationToken::new();
        // token NOT cancelled
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "flag=false and token not cancelled must proceed normally"
        );
    }

    #[tokio::test]
    async fn transient_control_status_error_does_not_cancel_run() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_run_id = Some("run-control-error".to_string());
        state.run_control = Some(Arc::new(FailingStatusRunControl));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("control-plane lookup failure should fail open");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "transient control status errors must not be treated as cancellation"
        );
        assert!(
            state.interruption.is_none(),
            "no cancellation interruption should be recorded for control-plane errors"
        );
    }

    #[tokio::test]
    async fn between_turn_control_status_poll_is_shared_for_cancel_and_pause_checks() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-clean".to_string());
        let provider = Arc::new(CountingStatusRunControl::new(None));
        state.run_control = Some(provider.clone());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("clean durable control state should proceed");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            provider.calls(),
            1,
            "between-turn cancel and pause checks should share one durable control poll"
        );
    }

    #[tokio::test]
    async fn between_turn_cross_pod_cancel_uses_single_control_poll() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-cancel".to_string());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        state.cancellation.flag = Some(cancel_flag.clone());
        let provider = Arc::new(CountingStatusRunControl::new(Some(
            RunControlStatus::Cancelled,
        )));
        state.run_control = Some(provider.clone());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("durable cancel should complete cleanly");

        assert!(matches!(
            prepared,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
        ));
        assert_eq!(
            provider.calls(),
            1,
            "durable cancel should not be followed by a second pause poll"
        );
        assert!(
            cancel_flag.load(Ordering::Acquire),
            "durable cancel should sync the same-pod cancel flag"
        );
    }

    #[tokio::test]
    async fn canonical_user_marker_wins_over_execution_lease_loss() {
        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-user-cancelled".to_string());
        state.cancellation.execution_lease_lost = Some(Arc::new(AtomicBool::new(true)));
        state.run_control = Some(Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_cancellation_origin(CancellationOrigin::User),
        ));

        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User,
            "the durable user marker is stronger evidence than the local lease fence it caused"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stalled_cancellation_origin_provider_is_bounded_and_unverified() {
        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-stalled-origin".to_string());
        state.run_control = Some(Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_pending_cancellation_origin(),
        ));

        let resolution = resolve_cancellation_origin(&mut state);
        tokio::pin!(resolution);
        tokio::select! {
            _ = &mut resolution => panic!("stalled origin lookup must not settle before its bound"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT).await;
        assert_eq!(resolution.await, CancellationOrigin::Unverified);
    }

    #[tokio::test]
    async fn durable_user_marker_upgrades_provisional_origin_and_never_downgrades() {
        for provisional in [CancellationOrigin::Runtime, CancellationOrigin::Unverified] {
            let mut state = make_state();
            state.context_manifest_user_id = Some("user-control".to_string());
            state.current_run_id = Some("run-origin-settlement".to_string());
            let provider = Arc::new(
                CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                    .with_cancellation_origin_sequence(provisional, CancellationOrigin::User),
            );
            state.run_control = Some(provider.clone());

            assert_eq!(resolve_cancellation_origin(&mut state).await, provisional);
            assert_eq!(
                resolve_cancellation_origin(&mut state).await,
                CancellationOrigin::User,
                "a durable User marker must upgrade {provisional:?} before terminal settlement"
            );
            assert_eq!(
                state.cancellation.resolved_origin,
                Some(CancellationOrigin::User),
                "terminal and descendant consumers share the upgraded User authority"
            );
            assert_eq!(provider.origin_calls(), 2);
            assert_eq!(
                resolve_cancellation_origin(&mut state).await,
                CancellationOrigin::User
            );
            assert_eq!(
                provider.origin_calls(),
                2,
                "canonical User authority is monotonic and needs no further lookup"
            );
        }

        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-user-origin-monotonic".to_string());
        let provider = Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_cancellation_origin_sequence(
                    CancellationOrigin::User,
                    CancellationOrigin::Runtime,
                ),
        );
        state.run_control = Some(provider.clone());
        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User
        );
        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User
        );
        assert_eq!(provider.origin_calls(), 1, "User must never be downgraded");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn paused_loop_uses_slow_durable_poll_but_keeps_local_cancel_fast() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.context_manifest_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-paused".to_string());
        let pause_flag = Arc::new(AtomicBool::new(true));
        let cancel_flag = Arc::new(AtomicBool::new(false));
        state.cancellation.pause_flag = Some(pause_flag);
        state.cancellation.flag = Some(cancel_flag.clone());
        let provider = Arc::new(CountingStatusRunControl::new(Some(
            RunControlStatus::Paused,
        )));
        state.run_control = Some(provider.clone());

        let mut prepared = Box::pin(prepare_turn_iteration(&mut host, &mut state, 0));

        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete before control changes"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(provider.calls(), 0);

        tokio::time::advance(PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL - Duration::from_millis(1))
            .await;
        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete before control changes"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            provider.calls(),
            0,
            "paused run should not poll durable control before the slow interval"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::advance(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete while durable status remains paused"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            provider.calls(),
            1,
            "paused run should poll durable control after the slow interval and one local tick"
        );

        cancel_flag.store(true, Ordering::Release);
        tokio::time::advance(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
        let prepared = prepared
            .await
            .expect("local cancel should exit paused loop");
        assert!(matches!(
            prepared,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
        ));
        assert_eq!(
            provider.calls(),
            1,
            "local cancel should not wait for another durable control poll"
        );
    }

    /// Runtime cancellation does not write a user interruption record.
    #[tokio::test]
    async fn cancellation_sets_interruption_record() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

        let _ = prepare_turn_iteration(&mut host, &mut state, 0).await;

        assert!(state.interruption.is_none());
    }

    // ── estimate_context_pressure tests ────────────────────────────
    //
    // Session 540c37d1: the old estimate_tokens (without schema tokens)
    // underestimated CJK tokens by ~50%, causing pressure to read 0.61
    // when it was actually 0.89. These tests ensure the unified function
    // returns correct pressure across all scenarios.

    #[test]
    fn estimate_context_pressure_zero_max_tokens() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let (pressure, tokens) = estimate_context_pressure(&messages, 0, 0);
        assert_eq!(pressure, 0.0, "zero max_tokens → zero pressure");
        assert_eq!(tokens, 0, "zero max_tokens → zero token count");
    }

    #[test]
    fn estimate_context_pressure_empty_messages() {
        let messages: Vec<serde_json::Value> = vec![];
        let (pressure, _tokens) = estimate_context_pressure(&messages, 0, 100_000);
        // estimate_tokens has a base overhead even for empty messages,
        // so pressure is non-zero. What matters: it's well below warning.
        assert!(pressure < 0.2, "empty messages pressure stays low");

        // With zero max_tokens: returns (0.0, 0) via guard.
        let (pressure_zero, tokens_zero) = estimate_context_pressure(&messages, 0, 0);
        assert_eq!(pressure_zero, 0.0);
        assert_eq!(tokens_zero, 0);
    }

    #[test]
    fn estimate_context_pressure_normal_ascii() {
        let messages = vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "What is 2+2?"}),
        ];
        let (pressure, tokens) = estimate_context_pressure(&messages, 5_000, 100_000);
        assert!(
            pressure > 0.0,
            "non-empty messages produce non-zero pressure"
        );
        assert!(
            pressure < 0.3,
            "short messages with schema well under budget; got {pressure}"
        );
        assert!(tokens > 0, "tokens must be > 0");
    }

    #[test]
    fn estimate_context_pressure_schema_tokens_raise_pressure() {
        let messages: Vec<serde_json::Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("message {}", i)}))
            .collect();
        let (p_without, _) = estimate_context_pressure(&messages, 0, 100_000);
        let (p_with, _) = estimate_context_pressure(&messages, 50_000, 100_000);
        assert!(
            p_with > p_without,
            "50K schema tokens must raise pressure above baseline"
        );
    }

    #[test]
    fn estimate_context_pressure_cjk_messages_count_tokens() {
        let messages: Vec<serde_json::Value> = (0..50)
            .map(|i| {
                json!({"role": "user", "content": format!("这是第{}条中文消息，包含较多的中文字符以确保token估算准确。", i)})
            })
            .collect();
        let (pressure, tokens) = estimate_context_pressure(&messages, 10_000, 100_000);
        assert!(tokens > 5_000, "50 CJK messages produce substantial tokens");
        assert!(
            pressure > 0.05,
            "50 CJK messages produce measurable pressure"
        );
    }

    #[test]
    fn estimate_context_pressure_scales_with_message_count() {
        let make_messages = |n: usize| -> Vec<serde_json::Value> {
            (0..n)
                .map(|i| json!({"role": "user", "content": format!("message number {}", i)}))
                .collect()
        };
        let (p10, _) = estimate_context_pressure(&make_messages(10), 0, 100_000);
        let (p50, _) = estimate_context_pressure(&make_messages(50), 0, 100_000);
        let (p100, _) = estimate_context_pressure(&make_messages(100), 0, 100_000);
        assert!(p100 > p50, "100 msgs > 50 msgs pressure");
        assert!(p50 > p10, "50 msgs > 10 msgs pressure");
        assert!(p100 > p10, "100 msgs > 10 msgs pressure");
    }

    #[test]
    fn structured_reanchor_updates_working_memory_before_turn() {
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "No, that's wrong; use the server-side executor.".into();
        {
            let memory = state
                .pipeline_session
                .as_mut()
                .expect("pipeline session")
                .working_memory_mut();
            memory.push_decision("keep durable project fact");
            memory.push_blocker("stale tool outage");
            memory.set_next_action("retry stale path");
        }

        assert!(apply_structured_user_reanchor(
            &mut state,
            astra_turn_types::ObjectiveRelation::Correct,
        ));

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("keep durable project fact"));
        assert!(!rendered.contains("stale tool outage"));
        assert!(!rendered.contains("retry stale path"));
        assert!(
            rendered.contains("Latest user correction overrides conflicting prior working memory")
        );
        assert!(rendered.contains("server-side executor"));
    }

    #[tokio::test]
    async fn prepare_turn_without_judged_reanchor_does_not_reanchor_working_memory() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "No, that's wrong; use the server-side executor.".into();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        {
            let memory = state
                .pipeline_session
                .as_mut()
                .expect("pipeline session")
                .working_memory_mut();
            memory.push_blocker("current blocker");
            memory.set_next_action("continue current path");
        }

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("current blocker"));
        assert!(rendered.contains("continue current path"));
    }

    #[test]
    fn user_correction_reanchors_transient_runtime_state_before_turn() {
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "不是修修补补，我要的是第一性原则系统性修复。".into();
        state.user_intent = state.message.clone();

        state.turn_guard.nudge_count = 4;
        state.turn_guard.record_tool_calls(&[
            json!({"function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"}}),
            json!({"function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"}}),
        ]);
        for _ in 0..3 {
            state
                .turn_guard
                .record_tool_result("bash", "Error: command timed out");
            state.turn_guard.health.record_failure("bash");
        }
        assert!(state.turn_guard.health.is_avoidance_advised("bash"));
        assert!(!state.turn_guard.tool_sigs.is_empty());
        assert!(state.turn_guard.errors.recent_error_pressure() > 0);

        state.restricted_tools.insert("bash".into());
        state.boosted_tools.insert("grep".into());

        assert!(apply_structured_user_reanchor(
            &mut state,
            astra_turn_types::ObjectiveRelation::Correct,
        ));

        assert_eq!(state.turn_guard.nudge_count, 0);
        assert!(state.turn_guard.tool_sigs.is_empty());
        assert_eq!(state.turn_guard.errors.recent_error_pressure(), 0);
        assert!(
            state.turn_guard.health.is_avoidance_advised("bash"),
            "durable tool diagnostics should remain available"
        );
        assert_eq!(
            state.restricted_tools,
            HashSet::from(["bash".to_string()]),
            "semantic re-anchoring must not broaden the hard capability surface"
        );
        assert!(
            state.boosted_tools.is_empty(),
            "stale auto-reflection boosts belong to the previous episode"
        );
        assert!(
            state.widen_selection_pending,
            "the next assembly should expose the full tool catalogue once"
        );
    }

    #[tokio::test]
    async fn prepare_turn_applies_structured_reanchor_from_judge() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct)
            .with_feedback(astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Correction,
                target: astra_turn_types::UserFeedbackTarget::Approach,
            });
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        state.telemetry.observability_session = Some(make_session());
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "不是修修补补，我要的是第一性原则系统性修复。".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        state.turn_guard.nudge_count = 2;
        state.restricted_tools.insert("bash".into());
        state.boosted_tools.insert("grep".into());
        state
            .pipeline_session
            .as_mut()
            .expect("pipeline session")
            .working_memory_mut()
            .set_next_action("stale path");

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");
        let repeated = prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .expect("same corrected user turn should remain preparable");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(matches!(repeated, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.turn_guard.nudge_count, 0);
        assert_eq!(state.restricted_tools, HashSet::from(["bash".to_string()]));
        assert!(state.boosted_tools.is_empty());
        assert!(state.widen_selection_pending);
        let signals = hub.recent_feedback_signals();
        let correction = signals
            .iter()
            .find(|signal| signal.signal_type == astra_core::feedback::SignalType::Correction)
            .expect("typed correction feedback");
        assert_eq!(
            signals
                .iter()
                .filter(|signal| {
                    signal.signal_type == astra_core::feedback::SignalType::Correction
                })
                .count(),
            1,
            "one user turn must emit one correction signal"
        );
        assert_eq!(correction.context["objective_relation"], "correct");
        assert_eq!(correction.context["feedback_kind"], "correction");
        assert_eq!(correction.context["feedback_target"], "approach");
        let session = state
            .telemetry
            .observability_session
            .as_ref()
            .expect("observability session");
        let session = astra_core::sync_poison::recover_rwlock_read(session);
        assert_eq!(session.user_corrections.len(), 1);
        assert_eq!(
            session
                .recent_correction_excerpts
                .last()
                .map(String::as_str),
            Some("不是修修补补，我要的是第一性原则系统性修复。")
        );
        assert_eq!(session.recent_correction_excerpts.len(), 1);
        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(!rendered.contains("stale path"));
        assert!(
            rendered.contains("Latest user correction overrides conflicting prior working memory")
        );
    }

    #[tokio::test]
    async fn prepare_turn_retains_typed_requirement_for_the_next_model_boundary() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Refine)
            .with_feedback(astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Requirement,
                target: astra_turn_types::UserFeedbackTarget::Verification,
            });
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "Run the online database journey before merging.".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("Latest user requirement for verification"));
        assert!(rendered.contains("Run the online database journey before merging."));
    }

    #[test]
    fn structured_correction_feedback_does_not_require_profile_session() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct);
        let mut state = make_state();
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        assert!(state.telemetry.observability_session.is_none());

        apply_judged_turn_intent_to_observability(&state, &intent, true);

        assert!(
            hub.recent_feedback_signals().iter().any(|signal| {
                signal.signal_type == astra_core::feedback::SignalType::Correction
            })
        );
    }

    #[test]
    fn acknowledgement_implies_acceptance_but_work_relations_do_not() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));

        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Continue),
            true,
        );
        assert!(hub.recent_feedback_signals().is_empty());
        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace),
            true,
        );
        assert!(hub.recent_feedback_signals().is_empty());

        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Acknowledge),
            true,
        );
        assert!(
            hub.recent_feedback_signals()
                .iter()
                .any(|signal| signal.signal_type == astra_core::feedback::SignalType::Acceptance)
        );
    }

    // ── Full pipeline integration test ─────────────────────────────
    //
    // Verifies that prepare_turn_iteration runs the complete compaction
    // pipeline — pressure estimation → warning emission → microcompact
    // → re-estimation → proactive compression — without panicking.

    fn high_pressure_cjk_state(max_tokens: u64, schema_tokens: usize) -> AgenticLoopState {
        let mut state = make_state();
        state.max_turn_input_tokens = max_tokens;
        state.pinned_tool_schema_tokens = schema_tokens as u64;
        state.messages = (0..150)
            .map(|i| {
                json!({"role": "user", "content": format!("这是第{}条测试消息，用于模拟高压力场景，包含足够多的中文字符来产生真实的token估算。会话540c37d1显示CJK文本的token估算往往被低估，这个测试确保修复后不会退化。", i)})
            })
            .collect();
        state
    }

    #[tokio::test]
    async fn prepare_turn_with_cjk_pressure_runs_full_pipeline() {
        // Small context window (32K) + CJK messages + schema tokens
        // pushes pressure well above the 0.70 trigger for ≤32K windows.
        let mut host = MockHost::new(Vec::new());
        let mut state = high_pressure_cjk_state(32_000, 10_000);

        let result = prepare_turn_iteration(&mut host, &mut state, 1).await;
        assert!(
            result.is_ok(),
            "prepare_turn_iteration must not fail under high pressure: {:?}",
            result.err()
        );

        // Messages should have been compacted (count reduced from 150).
        assert!(
            state.messages.len() < 150,
            "messages must be compacted under high pressure; got {}",
            state.messages.len()
        );
        // CJK + schema tokens at 32K window must trigger at least
        // CompactHistory-level compaction (significant reduction).
        assert!(
            state.messages.len() <= 100,
            "CJK session must be significantly compacted (≤100 from 150); got {}",
            state.messages.len()
        );

        // Compaction is already proven by message-count reduction above.
        // The `_compact_boundary` marker lives in `Message.extra` and is
        // intentionally stripped by `From<Message> for Value` to keep
        // prompt-cache prefixes stable on the provider wire — it does NOT
        // survive into `state.messages` (Vec<Value>). Asserting on it here
        // would test the wrong layer. See compaction_engine_tests.rs for
        // typed-level boundary marker assertions.

        assert!(
            state.context_compression_triggered,
            "quiet/headless rendering must not erase the turn-level compression fact"
        );
        assert!(
            !host.compaction_events.is_empty(),
            "quiet mode must preserve structured compaction callbacks"
        );
        let compaction_event = state
            .step_recorder
            .events()
            .iter()
            .find(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            })
            .expect(
                "quiet mode may suppress UI output, but durable compaction audit must be emitted",
            );
        let compaction_kind = compaction_event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("kind"))
            .and_then(serde_json::Value::as_str)
            .expect("production compaction paths must identify their strategy");
        assert!(
            matches!(
                compaction_kind,
                "proactive_default" | "proactive_aggressive"
            ),
            "unexpected compaction strategy: {compaction_kind}"
        );
    }

    #[tokio::test]
    async fn quiet_changes_only_compaction_rendering_not_typed_runtime_facts() {
        #[derive(Debug)]
        struct Capture {
            messages: Vec<serde_json::Value>,
            callbacks: Vec<CompactionEvent>,
            durable_facts: Vec<(
                astra_pipeline::step_protocol::StepEventType,
                Option<serde_json::Value>,
            )>,
            effectiveness: (u64, bool, u64, u32, u32),
            compression_triggered: bool,
            tier: CompactionTier,
            rendered_summaries: Vec<String>,
        }

        let mut captures = Vec::new();
        for quiet in [true, false] {
            let mut host = MockHost::new(Vec::new()).with_quiet(quiet);
            let mut state = high_pressure_cjk_state(32_000, 10_000);

            let prepared = prepare_turn_iteration(&mut host, &mut state, 1)
                .await
                .expect("the shared compaction input must be accepted");
            let PreparedTurnIteration::Ready(prep) = prepared else {
                panic!("compaction fixture must remain ready for provider execution");
            };
            assert_eq!(
                prep.quiet, quiet,
                "render policy must be the only varied input"
            );

            let durable_facts = state
                .step_recorder
                .events()
                .iter()
                .filter(|event| {
                    event.event_type
                        == astra_pipeline::step_protocol::StepEventType::CompactionFired
                })
                .map(|event| (event.event_type.clone(), event.payload.clone()))
                .collect();
            captures.push(Capture {
                messages: state.messages,
                callbacks: host.compaction_events,
                durable_facts,
                effectiveness: (
                    state.compaction_effectiveness.last_tokens_freed,
                    state.compaction_effectiveness.last_was_insufficient,
                    state.compaction_effectiveness.cumulative_tokens_freed,
                    state.compaction_effectiveness.attempt_count,
                    state.compaction_effectiveness.consecutive_futile_attempts,
                ),
                compression_triggered: state.context_compression_triggered,
                tier: state.compact_tier_applied,
                rendered_summaries: host.rendered_compaction_summaries,
            });
        }

        let quiet = &captures[0];
        let rendered = &captures[1];
        assert!(!quiet.callbacks.is_empty(), "fixture must actually compact");
        assert_eq!(quiet.callbacks, rendered.callbacks);
        assert_eq!(quiet.durable_facts, rendered.durable_facts);
        assert_eq!(quiet.effectiveness, rendered.effectiveness);
        assert_eq!(quiet.compression_triggered, rendered.compression_triggered);
        assert_eq!(quiet.tier, rendered.tier);
        assert_eq!(
            quiet.messages, rendered.messages,
            "quiet must not alter the compacted conversation"
        );

        assert!(
            quiet.rendered_summaries.is_empty(),
            "quiet suppresses rendering only"
        );
        assert_eq!(
            rendered.rendered_summaries,
            rendered
                .callbacks
                .iter()
                .map(|event| event.summary.clone())
                .collect::<Vec<_>>(),
            "rendered mode projects every typed callback without changing it"
        );
    }

    /// Resume-time compaction: when turn_index==0 and there are >10
    /// messages (e.g. restored from checkpoint), pressure estimation
    /// should trigger proactive compression before the first LLM call.
    #[tokio::test]
    async fn prepare_turn_resume_compacts_high_pressure() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 32_000;
        state.pinned_tool_schema_tokens = 10_000;
        state.messages = (0..100)
            .map(|i| {
                json!({"role": "user", "content": format!("CJK压力测试第{}条消息——确保恢复路径也能正常压缩上下文窗口。", i)})
            })
            .collect();

        // turn_index == 0 triggers resume compaction path
        let result = prepare_turn_iteration(&mut host, &mut state, 0).await;
        assert!(
            result.is_ok(),
            "resume compaction must not fail: {:?}",
            result.err()
        );
        assert!(
            state.messages.len() < 100,
            "resume compaction must reduce message count from 100; got {}",
            state.messages.len()
        );
    }

    #[tokio::test]
    async fn prepare_turn_with_low_pressure_skips_compaction() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 200_000; // large window
        state.messages = (0..5)
            .map(|i| json!({"role": "user", "content": format!("msg {}", i)}))
            .collect();

        let result = prepare_turn_iteration(&mut host, &mut state, 1).await;
        assert!(result.is_ok());

        // Low pressure should leave all 5 messages intact.
        assert_eq!(
            state.messages.len(),
            5,
            "low-pressure messages must not be compacted"
        );
    }
}
