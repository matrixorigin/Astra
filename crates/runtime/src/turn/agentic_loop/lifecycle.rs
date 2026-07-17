use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::super::{CompactionEngine, TokenBudget};
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, RunControlStatus,
    SkillAutoRouteJudgeContext, TurnIntentJudgeOutcome, try_write_heavy_checkpoint,
};
use crate::orchestration::permission_sync::PermissionResponseMessaging;
use crate::orchestration::{
    AgentToolRecordActionKind, project_agent_tool_budget_record,
    render_agent_tool_budget_unfinished_detail, summarize_agent_tool_budget_result,
};
use astra_config::user_profile::{Scenario, TurnIntent, WorkspaceMutationIntent};
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

fn apply_judged_turn_intent_to_observability_session(
    state: &AgenticLoopState,
    intent: &TurnIntent,
) {
    let Some(session) = &state.telemetry.observability_session else {
        return;
    };

    let mut session = astra_core::sync_poison::recover_rwlock_write(session);
    if let Some(scenario) = intent.requested_scenario {
        if !intent.prohibited_scenarios.contains(&scenario) {
            session.profile.set_scenario(scenario);
        }
    } else if session
        .profile
        .current_scenario
        .is_some_and(|scenario| intent.prohibited_scenarios.contains(&scenario))
    {
        session.profile.current_scenario = None;
        session.profile.touch();
    }
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
    astra_turn_core::runtime_scaffolding::strip_user_runtime_scaffolding_affixes(message)
}

async fn maybe_pre_route_skill<H: AgenticLoopHost>(host: &mut H, state: &mut AgenticLoopState) {
    if state.llm_rounds_completed > 0
        || !state.tool_results.is_empty()
        || !state.skills.invoked.is_empty()
    {
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
        tracing::warn!(
            query_length = query.len(),
            visible_skill_count = visible.len(),
            "skill auto-route judge failed; skipping auto-route for this turn"
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
    state.skills.invoked.insert(
        skill_name.clone(),
        crate::turn::skill_tool::InvokedSkill {
            name: skill_name,
            content: skill_result,
            invoked_at_turn: session_turn_number(state),
            reentry_count: 0,
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

    if completed.is_empty() || unfinished.is_empty() {
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

    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let mut lines = vec![
        format!(
            "[Turn budget exhausted after {} agentic turn(s). {} parallel sub-agent result(s) completed; {} did not finish before the turn ended.{}]",
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

fn unfinished_parallel_agent_ids(state: &AgenticLoopState) -> Vec<String> {
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
    agent_ids: Vec<String>,
    reason: &str,
) -> HashSet<String> {
    if agent_ids.is_empty() {
        return HashSet::new();
    }
    match tokio::time::timeout(
        CHILD_AGENT_CANCEL_TIMEOUT,
        host.cancel_child_agents(&agent_ids, reason),
    )
    .await
    {
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

/// Inject a one-shot "you have N turns left — wrap up" nudge into
/// the volatile lane when crossing budget thresholds (50 % and
/// 20 % remaining). Purpose: let a spawned agent that can't see
/// its own max_turns recognise when it's running short and start
/// finalising output instead of falling off the turn cliff with
/// nothing delivered. The signal piggybacks on
/// [`VolatileKind::BudgetAdvisory`] — one volatile slot per round,
/// drained by the bridge. Short, blunt wording so the model acts on
/// it rather than treating it as flavor text.
///
/// Guard against repeated emission: we only fire exactly once per
/// threshold crossing. The threshold watermarks live on the state
/// (`turn_budget_hint_emitted_50`, `..._20`). A budget extension
/// resets them so a freshly-extended budget gets the hints again
/// at the new thresholds.
fn maybe_emit_turn_budget_self_pacing_hint(state: &mut AgenticLoopState) {
    // Budgets of 3 turns or fewer aren't worth pacing — the hint
    // itself would be the largest part of the remaining work.
    if state.max_turns < 4 {
        return;
    }
    // `remaining_turns` was already decremented by the caller, so
    // this is the TRUE number remaining for this round and later.
    let remaining = state.remaining_turns;
    let max = state.max_turns;
    if max == 0 {
        return;
    }
    let pct_remaining = remaining * 100 / max;

    // 20 % crossing: hard nudge. 50 % crossing: soft nudge.
    // 90 %-remaining (≤10 % consumed) crossing: gentle early heads-up.
    // Emit the highest-priority (lowest %) threshold that newly
    // triggered, not multiple.
    if pct_remaining <= 20 && !state.turn_budget_hint_emitted_20 {
        state.turn_budget_hint_emitted_20 = true;
        state.turn_budget_hint_emitted_50 = true; // hoist so we don't re-emit 50 later
        state.turn_budget_hint_emitted_90 = true;
        let msg = format!(
            "[turn-budget] {remaining}/{max} turns remaining (≤20%). Wrap up now: write your final answer or last tool call. Further discovery will be cut off."
        );
        state.push_volatile(super::host::VolatileKind::BudgetAdvisory, msg);
    } else if pct_remaining <= 50 && !state.turn_budget_hint_emitted_50 {
        state.turn_budget_hint_emitted_50 = true;
        state.turn_budget_hint_emitted_90 = true;
        let msg = format!(
            "[turn-budget] {remaining}/{max} turns remaining (≤50%). Start converging: prioritise the deliverable over exploration."
        );
        state.push_volatile(super::host::VolatileKind::BudgetAdvisory, msg);
    } else if pct_remaining <= 90 && !state.turn_budget_hint_emitted_90 {
        state.turn_budget_hint_emitted_90 = true;
        let pct_consumed = 100 - pct_remaining;
        let msg = format!(
            "[turn-budget] {remaining}/{max} turns remaining (~{pct_consumed}% consumed). On track — continue, but if the task looks larger than this budget, consider creating a plan to split it into subtasks."
        );
        state.push_volatile(super::host::VolatileKind::BudgetAdvisory, msg);
    }
}

pub(crate) fn extract_tool_args(args: Option<&str>) -> Option<Value> {
    let args = args?;
    serde_json::from_str::<Value>(args).ok()
}

pub(crate) fn tool_record_is_workspace_mutation(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let args = extract_tool_args(record.args_full.as_deref())
        .or_else(|| extract_tool_args(record.args_preview.as_deref()));
    crate::turn::tool_side_effects::tool_call_invalidates_read_cache(&record.name, args.as_ref())
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

fn recent_activity_supports_budget_extension(state: &AgenticLoopState) -> bool {
    let recent_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .rev()
        .filter(|record| record.was_executed())
        .take(8)
        .collect();
    if recent_records.is_empty() {
        return false;
    }

    let successful_recent = recent_records.iter().any(|record| record.ok);
    if !successful_recent {
        return false;
    }

    let mutating_progress = recent_records
        .iter()
        .any(|record| record.ok && tool_record_is_workspace_mutation(record));
    let distinct_recent_turns = state
        .stall
        .turn_sigs
        .iter()
        .rev()
        .take(3)
        .filter(|sig| !sig.is_empty())
        .fold(
            Vec::<&std::collections::BTreeSet<String>>::new(),
            |mut acc, sig| {
                if !acc.contains(&sig) {
                    acc.push(sig);
                }
                acc
            },
        )
        .len();

    if recent_turns_are_repetitive(state) {
        return false;
    }

    mutating_progress || distinct_recent_turns >= 2
}

fn should_suggest_task_board_for_profile(
    profile: astra_turn_core::chat_turn_heuristics::TaskExecutionProfile,
) -> bool {
    profile.complexity == astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex
        || (profile.mutates_workspace && profile.exploratory_task)
}

async fn queue_task_board_advisory<H: AgenticLoopHost>(host: &H, state: &mut AgenticLoopState) {
    if !host.valid_tool_names().contains("task_board") {
        return;
    }

    state.refresh_task_board_snapshot().await;
    if state.hooks.task_board_snapshot.has_unfinished_tasks() {
        state.push_volatile_payload(
            super::host::VolatileKind::TaskBoardAdvisory,
            serde_json::json!({
                "schema": "task_board_advisory.v1",
                "signal": "unfinished_tasks_observed",
                "evidence": {
                    "tracked_count": state.hooks.task_board_snapshot.tracked_count,
                    "pending_count": state.hooks.task_board_snapshot.pending_count,
                    "in_progress_count": state.hooks.task_board_snapshot.in_progress_count,
                    "paused_count": state.hooks.task_board_snapshot.paused_count,
                    "active_tasks": state.hooks.task_board_snapshot.active_tasks,
                },
                "recommendation": "Reconcile the live task state before claiming that tracked work is complete.",
                "authority": "advisory_evidence_only",
            }),
        );
        return;
    }

    if !should_suggest_task_board_for_profile(state.task_profile) {
        return;
    }

    let complexity = match state.task_profile.complexity {
        astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard => "standard",
        astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex => "complex",
    };
    state.push_volatile_payload(
        super::host::VolatileKind::TaskBoardAdvisory,
        serde_json::json!({
            "schema": "task_board_advisory.v1",
            "signal": "broad_work_without_task_board",
            "evidence": {
                "complexity": complexity,
                "mutates_workspace": state.task_profile.mutates_workspace,
                "exploratory_task": state.task_profile.exploratory_task,
            },
            "recommendation": "A small durable task board may help track broad multi-step or delegated work. Use it when that improves execution clarity.",
            "authority": "advisory_evidence_only",
            "model_discretion": "This signal does not gate analysis, tool use, delegation, or completion.",
        }),
    );
}

fn maybe_extend_turn_budget(state: &mut AgenticLoopState) -> Option<()> {
    let budget = state.agentic_turn_budget;
    if budget.extension_turns == 0
        || budget.max_extensions == 0
        || state.max_turns >= budget.hard_turn_limit
        || used_budget_extensions(state) >= budget.max_extensions
        || crate::server::run::lifecycle::has_turn_verdict_warning(&state.stall.verdict_events)
        || !recent_activity_supports_budget_extension(state)
    {
        return None;
    }

    let additional_turns = budget
        .extension_turns
        .min(budget.hard_turn_limit.saturating_sub(state.max_turns));
    if additional_turns == 0 {
        return None;
    }

    let previous_max_turns = state.max_turns;
    let previous_remaining_turns = state.remaining_turns;
    state.max_turns += additional_turns;
    state.remaining_turns += additional_turns;
    // Fresh budget → fresh self-pacing thresholds. Without this,
    // a child that already emitted the 50 %/20 % hints at the
    // original budget would be silent through the extension and
    // crash off the new cliff with no warning.
    state.turn_budget_hint_emitted_50 = false;
    state.turn_budget_hint_emitted_20 = false;
    state.turn_budget_hint_emitted_90 = false;
    state.push_volatile_payload(
        super::host::VolatileKind::BudgetUpdate,
        serde_json::json!({
            "schema": "runtime_budget_update.v1",
            "event": "turn_budget_extended",
            "previous": {
                "max_turns": previous_max_turns,
                "remaining_turns": previous_remaining_turns,
            },
            "current": {
                "max_turns": state.max_turns,
                "remaining_turns": state.remaining_turns,
            },
            "additional_turns": additional_turns,
            "hard_turn_limit": budget.hard_turn_limit,
            "authority": "runtime_budget_fact",
        }),
    );
    Some(())
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

    let single_tool_streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    // Stall signal must fire AFTER nudge (nudge at 6, stall at 8+) to give the model
    // a chance to self-correct before declaring a stall.
    if single_tool_streak >= 8 {
        return StallDiagnosis {
            signal: Some(format!("single_tool_streak={single_tool_streak}")),
            summary: Some(format!(
                "{single_tool_streak} consecutive single-tool rounds without batching"
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

pub(crate) async fn run_loop_preamble<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
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

fn apply_structured_user_reanchor(state: &mut AgenticLoopState) -> bool {
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
        session
            .working_memory_mut()
            .apply_user_correction(&state.message);
    }
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
    quiet: bool,
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
    let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
    state
        .compaction_effectiveness
        .record_compaction(outcome.total_tokens_freed);
    if outcome.total_tokens_freed > 0 && !quiet {
        let messages_after = state.messages.len();
        let messages_removed = messages_before.saturating_sub(messages_after);
        let layer_descriptions: Vec<String> = outcome
            .layer_results
            .iter()
            .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
            .collect();
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
    // cancelled or the run is no longer paused.  Each iteration polls
    // the DB and resets the poll timer.  This replaces the prior
    // recursive `Box::pin(prepare_turn_iteration(...)).await` which
    // would stack-overflow during long pause windows.
    loop {
        let mut last_db_poll = tokio::time::Instant::now();

        while state
            .cancellation
            .pause_flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
        {
            // Check in-memory cancel flags (fast path, same pod)
            if state
                .cancellation
                .flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
                || state
                    .cancellation
                    .token
                    .as_ref()
                    .is_some_and(|t| t.is_cancelled())
            {
                try_write_heavy_checkpoint(state);
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::UserCancelled,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(state, None),
                ));
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Cancelled,
                ));
            }

            // Periodic DB poll for cross-pod cancel/pause
            if last_db_poll.elapsed() >= PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL {
                if let Some(ref rc) = state.run_control {
                    if let (Some(user_id), Some(run_id)) = (
                        state.context_manifest_user_id.as_deref(),
                        state.current_run_id.as_deref(),
                    ) {
                        match rc.control_status(user_id, run_id).await {
                            Ok(Some(RunControlStatus::Cancelled)) => {
                                if let Some(ref flag) = state.cancellation.flag {
                                    flag.store(true, Ordering::SeqCst);
                                }
                                if let Some(ref token) = state.cancellation.token {
                                    token.cancel();
                                }
                                try_write_heavy_checkpoint(state);
                                state.interruption = Some(InterruptionRecord::new(
                                    InterruptionKind::UserCancelled,
                                    ResumeAction::ContinueImmediately,
                                    interruption_state_summary(state, None),
                                ));
                                return Ok(PreparedTurnIteration::Finished(
                                    AgenticLoopOutcome::Cancelled,
                                ));
                            }
                            Ok(Some(RunControlStatus::Paused)) => {
                                // DB says paused, keep waiting — sync in-memory flag
                                if let Some(ref flag) = state.cancellation.pause_flag {
                                    flag.store(true, Ordering::SeqCst);
                                }
                            }
                            Ok(None) => {
                                // Run is no longer paused — clear in-memory flag and break
                                if let Some(ref flag) = state.cancellation.pause_flag {
                                    flag.store(false, Ordering::SeqCst);
                                }
                                break;
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
            try_write_heavy_checkpoint(state);
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::UserCancelled,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            return Ok(PreparedTurnIteration::Finished(
                AgenticLoopOutcome::Cancelled,
            ));
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

    if state.remaining_turns == 0 {
        if maybe_extend_turn_budget(state).is_some() {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "↻ Budget review — extended to {}/{} turns.",
                        state.max_turns, state.agentic_turn_budget.hard_turn_limit
                    ),
                );
            }
            state.final_text.clear();
            state.interruption = None;
        } else if should_complete_budget_exhaustion_gracefully(state) {
            try_write_heavy_checkpoint(state);
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::BudgetExhausted,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            let cancelled_agents = cancel_child_agents_with_timeout(
                host,
                unfinished_parallel_agent_ids(state),
                "parent turn budget exhausted",
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

    match state.rate_limit_cooldown.check_request(false) {
        crate::bridge::RateLimitAction::Proceed => {}
        crate::bridge::RateLimitAction::WaitAndRetry { delay_ms } => {
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
        crate::bridge::RateLimitAction::UseFallback { .. } => {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⏳ Rate limit cooldown — waiting 5s (no fallback model)…".into(),
                );
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        crate::bridge::RateLimitAction::Reject {
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
    maybe_emit_turn_budget_self_pacing_hint(state);
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
    match host.judge_turn_intent(state).await {
        TurnIntentJudgeOutcome::Intent(intent) => {
            apply_judged_turn_intent_to_observability_session(state, &intent);
            apply_judged_turn_intent_to_runtime_profile(state, &intent);
            if intent.reanchors_current_objective() {
                apply_structured_user_reanchor(state);
            }
        }
        TurnIntentJudgeOutcome::FixedDefault => {
            // The baseline profile is constructed once from deterministic
            // request heuristics. Do not retain or synthesize an LLM intent.
            state.turn_intent = None;
        }
        TurnIntentJudgeOutcome::Unavailable => {
            tracing::debug!("turn intent judge unavailable; preserving current runtime profile");
        }
    }
    queue_task_board_advisory(host, state).await;
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
                delivered_count += 1;
                let from_label = &msg.from.agent_id;
                host.on_agent_communication(astra_messaging::agent_communication_event(
                    &mailbox.address,
                    astra_messaging::AgentCommunicationDirection::Received,
                    msg,
                ));

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
                    astra_messaging::types::MessagePayload::Progress { status, detail, .. } => {
                        let extra = detail.as_deref().unwrap_or("");
                        push_mailbox_model_preview(
                            &mut parts,
                            format!("[{from_label} progress]: {status} {extra}"),
                        );
                    }
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
        state.skills.listing_message = if full.is_empty() {
            None
        } else {
            let agent_spawn_available = host
                .capabilities()
                .has(astra_turn_core::capability::Capability::AgentSpawner);
            crate::prompts::build_skill_listing_section_with_context_window_and_caps(
                &full,
                approximate_context_window_from_effective_input_budget(state.max_turn_input_tokens),
                agent_spawn_available,
            )
            .map(|section| {
                serde_json::json!({
                    "role": "system",
                    "content": section.text,
                })
            })
        };
    }
    maybe_pre_route_skill(host, state).await;

    if turn_index > 0 {
        // Inventory snapshots go through the structured volatile lane so
        // they stay out of `state.messages[]` — the wire layer drains
        // them into volatile_preamble for each LLM call.
        const INVENTORY_HEADER: &str = astra_turn_core::runtime_scaffolding::ALREADY_FETCHED_PREFIX;
        let inventory = state.semantic_dedup.context_inventory();
        if !inventory.is_empty() {
            state.push_volatile(
                super::host::VolatileKind::AlreadyFetched,
                format!("{INVENTORY_HEADER} (do NOT re-read/re-grep these)\n{inventory}"),
            );
        }
    }

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
        let (pressure, pressure_estimate_tokens) = estimate_context_pressure(
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
            host.maybe_pre_turn_compact(state, pressure, quiet).await;
        }

        // Pre-turn pressure warning: when context is near the model-adaptive
        // warning threshold, emit a non-intrusive advisory so the user knows
        // compaction is imminent.
        if pressure >= CompactionTier::pre_turn_warning(state.max_turn_input_tokens) && !quiet {
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
            astra_services::local_session_artifact_store()
                .session_dir(sid)
                .ok()
        });
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
            if !quiet {
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
            }
            state.step_recorder.record_compaction(
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
            run_proactive_compaction(
                post_mc_pressure,
                post_mc_tokens,
                state,
                quiet,
                host,
                kind,
                label,
            );
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
                quiet,
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

    use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
    use crate::turn::agentic_loop::host::tests::{
        MockHost, make_hub, make_session, make_state, text_result,
    };
    use crate::turn::agentic_loop::host::{TaskBoardSnapshot, VolatileKind};

    use super::*;

    #[test]
    fn runtime_decision_intent_strips_runtime_scaffolding_without_heuristics() {
        let message = "review changes on current branch\n\n<system-reminder>\n[session-resume:v1]\nResume interrupted work. Mention prompt optimization here.\n</system-reminder>";

        assert_eq!(
            pure_user_intent_for_runtime_decision(message),
            "review changes on current branch"
        );
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
        ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            args_full: Some(
                json!({
                    "path": "/tmp/sample.rs",
                    "start_line": start_line,
                    "end_line": end_line
                })
                .to_string(),
            ),
            round: Some(round),
            ..Default::default()
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
        state.stall.tool_call_records = vec![read_record(0, 1, 80), read_record(1, 81, 160)];
        state.stall.turn_sigs = vec![turn_sig("read_file:1-80"), turn_sig("read_file:81-160")];

        assert!(
            recent_activity_supports_budget_extension(&state),
            "successful non-repetitive read-only analysis should be eligible for continuation"
        );
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

    #[tokio::test]
    async fn profile_budget_extension_uses_recent_activity_not_stale_side_flags() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: 4,
            extension_turns: 2,
            max_extensions: 1,
        };
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![read_record(0, 1, 80), read_record(1, 81, 160)];
        state.stall.turn_sigs = vec![turn_sig("read_file:1-80"), turn_sig("read_file:81-160")];

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
            state.volatile_pending.iter().any(|entry| {
                entry.kind == VolatileKind::BudgetUpdate
                    && entry.payload["schema"] == "runtime_budget_update.v1"
                    && entry.payload["additional_turns"] == 2
                    && entry.payload["current"]["max_turns"] == 4
                    && entry.payload["current"]["remaining_turns"] == 2
                    && entry.payload["authority"] == "runtime_budget_fact"
            }),
            "budget review should explain the extension"
        );
    }

    struct AutoRouteResolver;

    impl SkillResolver for AutoRouteResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
            Ok(ResolvedSkill {
                name: name.to_string(),
                instructions: format!("Use the {name} workflow."),
                model: None,
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

    #[tokio::test]
    async fn prepare_advises_task_board_for_structured_complex_work() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::Refactoring)
            .with_workspace_mutation(WorkspaceMutationIntent::MustMutate);
        let mut host = MockHost::new(Vec::new())
            .with_valid_tools(&["task_board", "agent_fanout"])
            .with_turn_intent(intent);
        let mut state = make_state();
        state.message = "3 agents review这个分支的changes. 第一性原则，不考虑兼容".to_string();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");
        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));

        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::TaskBoardAdvisory)
            .expect("broad delegated review should receive task-board evidence");
        assert_eq!(advisory.payload["schema"], "task_board_advisory.v1");
        assert_eq!(advisory.payload["signal"], "broad_work_without_task_board");
        assert_eq!(advisory.payload["authority"], "advisory_evidence_only");
        assert_eq!(
            advisory.payload["model_discretion"],
            "This signal does not gate analysis, tool use, delegation, or completion."
        );
    }

    #[tokio::test]
    async fn prepare_surfaces_existing_task_board_state_without_start_advisory() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::Refactoring)
            .with_workspace_mutation(WorkspaceMutationIntent::MustMutate);
        let mut host = MockHost::new(Vec::new())
            .with_valid_tools(&["task_board", "agent_fanout"])
            .with_turn_intent(intent);
        let mut state = make_state();
        state.message = "multi-agent review current branch changes from first principles".into();
        state.user_intent = state.message.clone();
        state.hooks.task_board_snapshot = TaskBoardSnapshot {
            tracked_count: 1,
            pending_count: 1,
            in_progress_count: 0,
            reconcilable_in_progress_count: 0,
            paused_count: 0,
            completed_count: 0,
            terminal_non_success_count: 0,
            blocked_count: 0,
            active_tasks: vec!["task-1 Review branch [pending]".into()],
        };

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");
        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));

        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::TaskBoardAdvisory)
            .expect("existing task-board state should reach the model as evidence");
        assert_eq!(advisory.payload["signal"], "unfinished_tasks_observed");
        assert_eq!(advisory.payload["evidence"]["tracked_count"], 1);
        assert_eq!(advisory.payload["evidence"]["pending_count"], 1);
        assert_eq!(advisory.payload["authority"], "advisory_evidence_only");
    }

    #[test]
    fn task_board_start_advisory_uses_structured_profile_not_message_text() {
        let default_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        assert!(!should_suggest_task_board_for_profile(default_profile));

        let complex_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                false,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex,
            );
        assert!(should_suggest_task_board_for_profile(complex_profile));
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
    fn interruption_state_summary_records_trailing_single_tool_without_hard_block() {
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
        assert_eq!(
            summary.stall_signal.as_deref(),
            Some("single_tool_streak=9")
        );
        assert_eq!(
            summary.resume_restricted_tools,
            Vec::<String>::new(),
            "single-tool stall must not hard-block the executor needed to finish"
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

        assert!(text.contains("Turn budget exhausted"), "{text}");
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
    async fn budget_exhaustion_wrapup_cancels_unfinished_parallel_agents() {
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
            .expect("budget exhaustion should finish cleanly");

        assert!(matches!(
            prepared,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.cancelled_agent_ids, vec!["agent-c".to_string()]);
        assert!(
            state.final_text.contains("parent cancelled this sub-agent"),
            "{}",
            state.final_text
        );
    }

    #[tokio::test(start_paused = true)]
    async fn child_agent_cancel_timeout_does_not_block_budget_wrapup() {
        let mut host = MockHost::new(Vec::new())
            .with_cancel_child_agents_delay(CHILD_AGENT_CANCEL_TIMEOUT + Duration::from_secs(30));

        let cancelled = cancel_child_agents_with_timeout(
            &mut host,
            vec!["agent-c".to_string()],
            "parent turn budget exhausted",
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
                .is_some_and(|content| {
                    content.contains(
                        "only these request-allowlisted non-skill tools are callable: read_file",
                    )
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
    async fn prepare_turn_iteration_pre_routes_structured_user_intent_not_prompt_scaffolding() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "<project-instructions>\nMention prompt optimization here.\n</project-instructions>\n\nreview changes on current branch".into();
        state.user_intent = "review changes on current branch".into();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));

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
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation),
            Some(WorkspaceMutationIntent::ReadOnly)
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

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_infer_code_review_without_judge_intent() {
        let mut host = MockHost::new(Vec::new());
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "please inspect the current changes".into();
        state.user_intent = state.message.clone();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(&state.message);
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        assert_ne!(guard.profile.current_scenario, Some(Scenario::CodeReview));
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_infer_quick_answer_without_judge_intent() {
        let mut host = MockHost::new(Vec::new());
        let hub = make_hub();
        let session = make_session();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub);
        state.telemetry.observability_session = Some(session.clone());
        state.message = "where is the auth flow defined?".into();
        state.user_intent = state.message.clone();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(&state.message);
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        assert_ne!(guard.profile.current_scenario, Some(Scenario::QuickAnswer));
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
        assert_eq!(
            host.skill_auto_route_queries,
            vec!["review changes on current branch".to_string()]
        );
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

    /// 90% hint must compute the consumed percentage dynamically —
    /// small budgets (e.g. max=4) can skip from 100% to 75% remaining in
    /// one turn, so hardcoding "~10% consumed" is misleading.
    #[test]
    fn budget_hint_90_reports_actual_consumption_not_hardcoded() {
        let mut state = make_state();
        state.max_turns = 4;
        state.remaining_turns = 3;
        state.turn_budget_hint_emitted_90 = false;
        state.turn_budget_hint_emitted_50 = false;
        state.turn_budget_hint_emitted_20 = false;
        state.volatile_pending.clear();

        maybe_emit_turn_budget_self_pacing_hint(&mut state);

        assert!(
            state.turn_budget_hint_emitted_90,
            "90% hint should fire when pct_remaining <= 90%"
        );

        let msg = state
            .volatile_pending
            .iter()
            .find(|inj| {
                inj.payload
                    .as_str()
                    .is_some_and(|text| text.contains("[turn-budget]"))
            })
            .and_then(|inj| inj.payload.as_str())
            .expect("expected budget hint in volatile_pending");

        assert!(
            !msg.contains("~10% consumed"),
            "must not hardcode ~10% consumed — remaining={}, max={}; msg: {msg}",
            state.remaining_turns,
            state.max_turns,
        );

        // 3/4 = 75% remaining → ~25% consumed
        assert!(
            msg.contains("~25% consumed") || msg.contains("75% remaining"),
            "expected actual consumption in message; msg: {msg}",
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
        calls: AtomicUsize,
    }

    impl CountingStatusRunControl {
        fn new(status: Option<RunControlStatus>) -> Self {
            Self {
                status,
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
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
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
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
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    /// In-memory AtomicBool flag set → immediate cancel, no DB poll needed.
    #[tokio::test]
    async fn in_memory_flag_cancellation_returns_cancelled() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

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
        let ir = state
            .interruption
            .as_ref()
            .expect("interruption must be set on cancel");
        assert_eq!(
            ir.kind,
            astra_turn_core::interruption::InterruptionKind::UserCancelled,
            "interruption kind must be UserCancelled"
        );
        assert_eq!(
            ir.resume_action,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        );
    }

    /// CancellationToken already cancelled → immediate cancel.
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
        let ir = state
            .interruption
            .as_ref()
            .expect("interruption must be set on cancel");
        assert_eq!(
            ir.kind,
            astra_turn_core::interruption::InterruptionKind::UserCancelled,
        );
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
            state.interruption.is_some(),
            "interruption must be set when cancel wins inside pause loop"
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
        assert!(state.interruption.is_some());
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

    /// Cancel writes an InterruptionRecord with correct kind and resume action.
    #[tokio::test]
    async fn cancellation_sets_interruption_record() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

        let _ = prepare_turn_iteration(&mut host, &mut state, 0).await;

        let ir = state
            .interruption
            .as_ref()
            .expect("interruption must be set after cancel");
        assert_eq!(
            ir.kind,
            astra_turn_core::interruption::InterruptionKind::UserCancelled
        );
        assert_eq!(
            ir.resume_action,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately
        );
        // user_message must be present for resume context
        assert!(
            !ir.user_message.is_empty(),
            "interruption user_message must be non-empty for resume guidance"
        );
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

        assert!(apply_structured_user_reanchor(&mut state));

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

        assert!(apply_structured_user_reanchor(&mut state));

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
        let intent = TurnIntent::default().with_reanchors_current_objective(true);
        let mut host = MockHost::new(Vec::new()).with_turn_intent(intent);
        let mut state = make_state();
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

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.turn_guard.nudge_count, 0);
        assert_eq!(state.restricted_tools, HashSet::from(["bash".to_string()]));
        assert!(state.boosted_tools.is_empty());
        assert!(state.widen_selection_pending);
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

        // Emissions are suppressed when quiet=true, but the pipeline
        // itself must execute without panicking — that's the contract.
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
