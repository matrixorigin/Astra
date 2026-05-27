use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::agentic::adaptive_tuning::apply_adaptive_execution_profile;
use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, try_write_heavy_checkpoint,
};
use crate::orchestration::permission_sync::PermissionResponseMessaging;
use astra_services::SessionArtifactStore;
use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::interruption::{
    InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
};
use astra_turn_core::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;

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
    if state.session_turn > 0 {
        state.session_turn
    } else {
        state.max_turns.saturating_sub(state.remaining_turns).max(1) as u32
    }
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
        .filter(|record| !record.is_synthetic_placeholder())
        .count()
        .min(u32::MAX as usize) as u32
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

fn parse_embedded_json(raw: Option<&str>) -> Option<Value> {
    raw.and_then(|text| serde_json::from_str::<Value>(text).ok())
}

fn agent_id_from_record(
    record: &astra_services::session_journal::ToolCallRecord,
    parsed_result: Option<&Value>,
) -> Option<String> {
    record
        .args_full
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| {
            value
                .get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            parsed_result
                .and_then(|value| value.get("agent_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn summarize_agent_result(text: &str) -> String {
    const MAX_CHARS: usize = 320;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        trimmed.to_string()
    } else {
        let mut clipped: String = trimmed.chars().take(MAX_CHARS.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    }
}

fn summarize_control_error(error: &str) -> String {
    if error.contains("duplicate_within_turn") {
        "same-turn retries hit duplicate_within_turn".to_string()
    } else if error.contains("blocked_tool") {
        "later retries were blocked after the tool was restricted".to_string()
    } else {
        error.lines().next().unwrap_or(error).trim().to_string()
    }
}

fn summarize_incomplete_agent_state(parsed: &Value) -> Option<String> {
    match parsed.get("status").and_then(Value::as_str) {
        Some("still_running") => {
            let detail = parsed
                .get("current_status")
                .and_then(Value::as_str)
                .unwrap_or("still running");
            Some(format!(
                "still running when the wait window expired ({detail})"
            ))
        }
        Some("timeout") => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("timed out while waiting for the child result")
                .to_string(),
        ),
        Some("failed") => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("child result retrieval failed")
                .to_string(),
        ),
        Some("unknown") => Some(
            parsed
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("child agent returned an unknown status")
                .to_string(),
        ),
        _ => None,
    }
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
        let action = record.args_preview.as_deref().unwrap_or_default();
        let parsed_result = parse_embedded_json(
            record
                .result_full
                .as_deref()
                .or(record.result_preview.as_deref()),
        );

        match action {
            "spawn" => {
                let Some(agent_id) = agent_id_from_record(record, parsed_result.as_ref()) else {
                    continue;
                };
                if !order.iter().any(|id| id == &agent_id) {
                    order.push(agent_id.clone());
                }
                let entry = summaries.entry(agent_id).or_default();
                if entry.label.is_none() {
                    entry.label = parsed_result
                        .as_ref()
                        .and_then(|value| value.get("description"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            record
                                .args_full
                                .as_deref()
                                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                                .and_then(|value| {
                                    value
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                })
                        });
                }
            }
            "get_result" => {
                let Some(agent_id) = agent_id_from_record(record, parsed_result.as_ref()) else {
                    continue;
                };
                if !order.iter().any(|id| id == &agent_id) {
                    order.push(agent_id.clone());
                }
                let entry = summaries.entry(agent_id).or_default();
                if let Some(error) = record.error.as_deref() {
                    let summarized = summarize_control_error(error);
                    if !entry
                        .control_errors
                        .iter()
                        .any(|existing| existing == &summarized)
                    {
                        entry.control_errors.push(summarized);
                    }
                }
                if let Some(parsed) = parsed_result.as_ref() {
                    match parsed.get("status").and_then(Value::as_str) {
                        Some("completed") | Some("interrupted") => {
                            if let Some(result) = parsed.get("result").and_then(Value::as_str) {
                                entry.completed_result = Some(result.to_string());
                            }
                        }
                        _ => {
                            if entry.completed_result.is_none() {
                                entry.incomplete_reason = summarize_incomplete_agent_state(parsed);
                            }
                        }
                    }
                }
            }
            _ => {}
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
            summarize_agent_result(&entry.result)
        ));
    }
    lines.push(String::new());
    lines.push("Unfinished sub-agent results:".to_string());
    for (idx, entry) in rollup.unfinished.iter().enumerate() {
        let mut detail = entry
            .incomplete_reason
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| "did not finish before the turn budget was exhausted".to_string());
        if cancelled_agents.contains(&entry.agent_id) {
            detail.push_str(
                "; the parent turn budget was exhausted and the parent cancelled this sub-agent",
            );
        }
        if !entry.control_errors.is_empty() {
            detail.push_str("; ");
            detail.push_str(&entry.control_errors.join("; "));
        }
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

pub(crate) fn extract_bash_command(args: Option<&str>) -> Option<String> {
    let value = extract_tool_args(args)?;
    let command = value.get("command").and_then(Value::as_str)?;
    Some(command.to_string())
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

fn recent_progress_is_real(state: &AgenticLoopState) -> bool {
    if is_open_ended_file_exploration(&state.message) {
        return state
            .stall
            .tool_call_records
            .iter()
            .rev()
            .take(6)
            .any(tool_record_is_workspace_mutation);
    }

    // Note: `tool_record_is_workspace_mutation` no longer treats every `bash`
    // call as mutating — only commands that actually modify state qualify.
    // This is intentional: a loop that only runs `grep`/`cat`/`ls` should not
    // earn budget extensions, since "spinning on read-only inspection without
    // committing changes" is exactly the failure mode we are trying to break.
    let recent_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .rev()
        .filter(|record| !record.is_synthetic_placeholder())
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

    if state.task_profile.mutates_workspace {
        return mutating_progress;
    }

    if state.task_profile.exploratory_task
        || state.task_profile.complexity
            == astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex
    {
        return mutating_progress || distinct_recent_turns >= 2;
    }

    false
}

const OPEN_ENDED_EXPLORATION_MAX_TURNS: usize = 4;
const OPEN_ENDED_EXPLORATION_MAX_TOOLS_PER_TURN: u32 = 2;
const OPEN_ENDED_EXPLORATION_BUDGET_MESSAGE: &str = "Open-ended file exploration budget is active: do at most one bounded useful pass with no more than two tool calls per turn, then summarize. Do not keep listing/reading files recursively.";

fn is_open_ended_file_exploration(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let has_list_operation =
        lower.contains("list") || lower.contains("ls ") || lower.contains("directory");
    let has_read_operation = lower.contains("read") || lower.contains("cat ");
    let asks_for_many_files = lower.contains("as many files") || lower.contains("many files as");
    let repeated_file_loop = has_list_operation
        && has_read_operation
        && (lower.contains("keep going")
            || lower.contains("as many")
            || lower.contains("list again")
            || lower.contains("read again"));
    let explicit_file_loop = asks_for_many_files
        || lower.contains("as many")
            && lower.contains("files")
            && (has_read_operation || has_list_operation);
    repeated_file_loop || explicit_file_loop
}

fn apply_open_ended_exploration_budget(state: &mut AgenticLoopState) -> bool {
    if !is_open_ended_file_exploration(&state.message) {
        return false;
    }

    state.max_turns = state.max_turns.min(OPEN_ENDED_EXPLORATION_MAX_TURNS);
    state.remaining_turns = state.remaining_turns.min(state.max_turns);
    state.max_tools_per_turn = state
        .max_tools_per_turn
        .min(OPEN_ENDED_EXPLORATION_MAX_TOOLS_PER_TURN);
    // Previously-pushed exploration-budget messages lived in
    // state.messages; no-op today because the structured lane drains
    // per-call and the old `any(|m| content == MSG)` guard never finds
    // them. Keep the single-push-per-turn semantics via the lane: if
    // any prior injection this turn already queued the message, skip.
    let already_queued = state
        .volatile_pending
        .iter()
        .any(|inj| inj.content == OPEN_ENDED_EXPLORATION_BUDGET_MESSAGE);
    if !already_queued {
        state.push_volatile(
            super::host::VolatileKind::ExplorationBudget,
            OPEN_ENDED_EXPLORATION_BUDGET_MESSAGE,
        );
    }
    true
}

fn maybe_extend_turn_budget(state: &mut AgenticLoopState) -> Option<String> {
    let budget = state.agentic_turn_budget;
    if budget.extension_turns == 0
        || budget.max_extensions == 0
        || state.max_turns >= budget.hard_turn_limit
        || used_budget_extensions(state) >= budget.max_extensions
        || crate::server::run::lifecycle::has_turn_verdict_warning(&state.stall.verdict_events)
        || !recent_progress_is_real(state)
    {
        return None;
    }

    let additional_turns = budget
        .extension_turns
        .min(budget.hard_turn_limit.saturating_sub(state.max_turns));
    if additional_turns == 0 {
        return None;
    }

    state.max_turns += additional_turns;
    state.remaining_turns += additional_turns;
    // Fresh budget → fresh self-pacing thresholds. Without this,
    // a child that already emitted the 50 %/20 % hints at the
    // original budget would be silent through the extension and
    // crash off the new cliff with no warning.
    state.turn_budget_hint_emitted_50 = false;
    state.turn_budget_hint_emitted_20 = false;
    state.turn_budget_hint_emitted_90 = false;
    let review_message = format!(
        "[Budget review] Recent progress looks real for this {}task, so continuing with {} extra turn(s). Hard limit: {} total turns.",
        if state.task_profile.exploratory_task {
            "exploratory "
        } else if state.task_profile.mutates_workspace {
            "implementation "
        } else {
            ""
        },
        additional_turns,
        budget.hard_turn_limit,
    );
    state.push_volatile(
        super::host::VolatileKind::BudgetReview,
        review_message.clone(),
    );
    Some(review_message)
}

/// Build an interruption state summary from the current loop state.
pub(crate) fn interruption_state_summary(
    state: &AgenticLoopState,
    error_detail: Option<String>,
) -> InterruptionStateSummary {
    let stall_signal = interruption_stall_signal(state);
    InterruptionStateSummary {
        has_checkpoint: state.stall.last_heavy_checkpoint.is_some(),
        tool_calls_completed: completed_tool_calls(state),
        turns_completed: current_agentic_step(state),
        remaining_turns: state.remaining_turns as u32,
        error_detail,
        stall_signal,
    }
}

pub(crate) fn interruption_diagnosis_summary(state: &AgenticLoopState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some((family, streak)) =
        astra_turn_core::evaluation::exploration_family_round_streak(&state.stall.tool_call_records)
        && streak >= astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
    {
        parts.push(match family {
            "read" => format!("{streak} consecutive read-dominant exploratory rounds"),
            "search" => format!("{streak} consecutive search-dominant exploratory rounds"),
            "diff" => format!("{streak} consecutive diff-dominant exploratory rounds"),
            _ => format!("{streak} consecutive {family}-dominant exploratory rounds"),
        });
    }
    let redundant_reads = astra_turn_core::evaluation::count_redundant_overlapping_reads(
        &state.stall.tool_call_records,
    );
    if redundant_reads >= astra_turn_core::evaluation::REDUNDANT_OVERLAPPING_READS_THRESHOLD {
        parts.push(format!(
            "{redundant_reads} redundant overlapping reads on unchanged files"
        ));
    }
    let single_tool_streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    if single_tool_streak >= 3 && parts.is_empty() {
        parts.push(format!(
            "a single-tool streak of {single_tool_streak} consecutive rounds"
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn interruption_stall_signal(state: &AgenticLoopState) -> Option<String> {
    if let Some((family, streak)) =
        astra_turn_core::evaluation::exploration_family_round_streak(&state.stall.tool_call_records)
        && streak >= astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
    {
        return Some(format!("exploration_family={family};streak={streak}"));
    }
    let redundant_reads = astra_turn_core::evaluation::count_redundant_overlapping_reads(
        &state.stall.tool_call_records,
    );
    if redundant_reads >= astra_turn_core::evaluation::REDUNDANT_OVERLAPPING_READS_THRESHOLD {
        return Some(format!("redundant_reads={redundant_reads}"));
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    (streak >= 3).then(|| format!("single_tool_streak={streak}"))
}

pub(crate) async fn run_loop_preamble<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    apply_open_ended_exploration_budget(state);

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
            state.messages.insert(
                0,
                serde_json::json!({
                    "role": "system",
                    "content": format!("[Session hooks]\n{ctx}"),
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

pub(crate) async fn prepare_turn_iteration<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
) -> Result<PreparedTurnIteration, String> {
    let quiet = host.is_quiet();

    while state
        .cancellation
        .pause_flag
        .as_ref()
        .is_some_and(|f| f.load(Ordering::Acquire))
    {
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

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
            let cancelled_agents: HashSet<String> = host
                .cancel_child_agents(
                    &unfinished_parallel_agent_ids(state),
                    "parent turn budget exhausted",
                )
                .await
                .into_iter()
                .collect();
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
    state.step_recorder.begin_turn_with_context(
        session_turn_number(state).saturating_sub(1),
        turn_index as u32,
    );

    if let Some(ref mut adapter) = state.tactical_adapter {
        adapter.reset_turn();
    }
    if let Some(ref mut collector) = state.step_signal_collector {
        collector.reset(state.max_turn_input_tokens);
    }

    let turn_start_time = Instant::now();

    // Initialize turn event buffer for fine-grained observability (once per turn).
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
            let s = session.read().unwrap_or_else(|e| e.into_inner());
            s.user_id.clone()
        };
        crate::observability::on_turn_start(hub, session_id, &user_id, &state.message);
    }
    apply_adaptive_execution_profile(state);

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
            for msg in &pending {
                let from_label = &msg.from.agent_id;

                match &msg.payload {
                    astra_messaging::types::MessagePayload::Ack { message_id } => {
                        if let Some(ref tracker) = state.messaging.ack_tracker {
                            tracker.acknowledge(message_id).await;
                        }
                        if let Some(ref metrics) = state.messaging.metrics {
                            metrics.acks_received.fetch_add(1, Ordering::Relaxed);
                        }
                        parts.push(format!(
                            "[{from_label} ack]: message {message_id} acknowledged"
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
                    if let Err(e) = mailbox.send(ack_reply).await {
                        astra_core::agent_warn!("mailbox", "Failed to send ack: {e}");
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
                    if let Err(e) = mailbox.send(response_msg).await {
                        astra_core::agent_warn!(
                            "mailbox",
                            "Failed to send permission response: {e}"
                        );
                    }
                    continue;
                }

                match &msg.payload {
                    astra_messaging::types::MessagePayload::Text { content, .. } => {
                        parts.push(format!("[{from_label}]: {content}"));
                    }
                    astra_messaging::types::MessagePayload::Progress { status, detail, .. } => {
                        let extra = detail.as_deref().unwrap_or("");
                        parts.push(format!("[{from_label} progress]: {status} {extra}"));
                    }
                    astra_messaging::types::MessagePayload::Request { request_type, .. } => {
                        parts.push(format!("[{from_label} request]: {request_type:?}"));
                    }
                    astra_messaging::types::MessagePayload::Response { accepted, .. } => {
                        parts.push(format!("[{from_label} response]: accepted={accepted}"));
                    }
                    astra_messaging::types::MessagePayload::Signal(sig) => {
                        parts.push(format!("[{from_label} signal]: {sig:?}"));
                    }
                    astra_messaging::types::MessagePayload::Ack { .. } => {}
                    astra_messaging::types::MessagePayload::Nack { .. } => {}
                }
            }
            if !parts.is_empty() {
                let mailbox_text = format!(
                    "📬 Messages from other agents ({}{}):\n{}",
                    pending.len(),
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
        // `build_skill_listing_section_for_model` (CacheScope::Session)
        // — the model id sizes the budget so smaller-context providers
        // don't waste prompt space on full listings.
        //
        // We still populate `listing_message` as a rendered `role: system`
        // value for downstream adapters (introspect tooling, tests) so
        // they don't need to know about the cache-scope plumbing.
        let full = resolver.available_skills();
        let model_hint = state.current_model_hint().map(str::to_string);
        state.skills.listing_message = if full.is_empty() {
            None
        } else {
            if state.telemetry.initial_skill_selector_shortlist.is_none() {
                state.telemetry.initial_skill_selector_shortlist = Some(());
            }
            let agent_spawn_available = host
                .capabilities()
                .has(astra_turn_core::capability::Capability::AgentSpawner);
            crate::prompts::build_skill_listing_section_with_caps(
                &full,
                model_hint.as_deref(),
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

    if turn_index > 0 {
        // Inventory snapshots go through the structured volatile lane so
        // they stay out of `state.messages[]` — the wire layer drains
        // them into volatile_preamble for each LLM call. Legacy
        // retains() stay for a grace period to scrub checkpoints
        // restored from pre-migration sessions (working-set / attention
        // manifests were removed in wip-3).
        const LEGACY_WORKING_SET_HEADER: &str = "[working-set:v1]\n";
        const LEGACY_ATTENTION_HEADER: &str = "[attention:v1]\n";
        state.messages.retain(|m| {
            let role = m.get("role").and_then(Value::as_str);
            let content = m.get("content").and_then(Value::as_str);
            match (role, content) {
                (Some("system"), Some(c)) if c.starts_with(LEGACY_WORKING_SET_HEADER) => false,
                (Some("user"), Some(c)) if c.starts_with(LEGACY_ATTENTION_HEADER) => false,
                _ => true,
            }
        });

        const INVENTORY_HEADER: &str = "## Already Fetched (do NOT re-read/re-grep these)\n";
        state.messages.retain(|m| {
            m.get("role").and_then(Value::as_str) != Some("system")
                || !m
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.starts_with(INVENTORY_HEADER))
        });
        let inventory = state.semantic_dedup.context_inventory();
        if !inventory.is_empty() {
            state.push_volatile(
                super::host::VolatileKind::AlreadyFetched,
                format!("{INVENTORY_HEADER}{inventory}"),
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
                let error_tools: Vec<&str> = state.turn_guard.health.deprioritized_tools();
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
        let pressure = if state.max_turn_input_tokens > 0 {
            let fresh_estimate = crate::prompts::estimate_tokens_precise(
                &state.messages,
                state.pinned_tool_schema_tokens as usize,
                0,
            ) as u64;
            fresh_estimate as f64 / state.max_turn_input_tokens as f64
        } else {
            0.0
        };

        // Pre-turn LLM compact: if pressure exceeds 80%, let the host run an
        // optional cache-friendly inline-summary pass before the next LLM call.
        // Server hosts can build the exact system prompt + history prefix;
        // generic hosts keep the default no-op behavior. When it succeeds the
        // host bumps `state.compact_tier_applied` so the later budget guard
        // won't re-run mechanical compression.
        if pressure >= 0.80
            && state.compact_tier_applied < CompactionTier::CompactHistory
            && state.messages.len() > 10
        {
            host.maybe_pre_turn_compact(state, pressure, quiet).await;
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
                host.emit_headless_line(
                    HeadlessStderrStyle::Dim,
                    format!(
                        "  ♻ Compacted {} old tool result(s), ~{} tokens saved (pressure {:.0}%)",
                        mc.results_compacted,
                        mc.tokens_saved,
                        pressure * 100.0,
                    ),
                );
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
        let post_mc_tokens = crate::prompts::estimate_tokens(&state.messages) as u64;
        let post_mc_pressure = if state.max_turn_input_tokens > 0 {
            post_mc_tokens as f64 / state.max_turn_input_tokens as f64
        } else {
            0.0
        };

        // Proactive compression gate: if pressure is still high after
        // microcompact, run the full compression pipeline *before* calling
        // the LLM, preventing 413 errors instead of reacting to them.
        if post_mc_pressure >= 0.75 {
            let budget = super::super::context_compression::TokenBudget {
                max_prompt_tokens: state.max_turn_input_tokens,
                last_measured_tokens: post_mc_tokens,
                current_round_index: Some(state.current_round_index),
            };
            let pipeline = if post_mc_pressure >= 0.90 {
                super::super::context_compression::CompressionPipeline::aggressive_pipeline()
            } else {
                super::super::context_compression::CompressionPipeline::default_pipeline()
            };
            let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
            if outcome.total_tokens_freed > 0 && !quiet {
                let tier = if post_mc_pressure >= 0.90 {
                    "aggressive"
                } else {
                    "default"
                };
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "  ⚡ Proactive {} compression: freed ~{} tokens at {:.0}% pressure",
                        tier,
                        outcome.total_tokens_freed,
                        post_mc_pressure * 100.0,
                    ),
                );
                // Record compression audit for pipeline journal
                if let Some(ref mut sess) = state.pipeline_session {
                    sess.record_compaction_audit(
                        if post_mc_pressure >= 0.90 {
                            "aggressive_compression"
                        } else {
                            "default_compression"
                        },
                        outcome.layer_results.len() as u32,
                        outcome.total_tokens_freed.min(u32::MAX as u64) as u32,
                    );
                    sess.stats.record_compaction(outcome.total_tokens_freed);
                }
            }
        }
    }

    // ── Compaction-on-resume: if turn 0 has many messages (restored from
    // checkpoint), estimate context pressure from raw content size and
    // proactively compress before the first LLM call.  This prevents an
    // immediate 413 when resuming from a CompactAndRetry interruption.
    if turn_index == 0 && state.messages.len() > 10 && state.max_turn_input_tokens > 0 {
        let estimated_tokens = crate::prompts::estimate_tokens(&state.messages) as f64;
        let estimated_pressure = estimated_tokens / state.max_turn_input_tokens as f64;
        if estimated_pressure >= 0.75 {
            let budget = super::super::context_compression::TokenBudget {
                max_prompt_tokens: state.max_turn_input_tokens,
                last_measured_tokens: estimated_tokens as u64,
                current_round_index: Some(state.current_round_index),
            };
            let pipeline = if estimated_pressure >= 0.90 {
                super::super::context_compression::CompressionPipeline::aggressive_pipeline()
            } else {
                super::super::context_compression::CompressionPipeline::default_pipeline()
            };
            let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
            if outcome.total_tokens_freed > 0 && !quiet {
                let tier = if estimated_pressure >= 0.90 {
                    "aggressive"
                } else {
                    "default"
                };
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "  ⚡ Resume {} compression: freed ~{} tokens at ~{:.0}% est. pressure",
                        tier,
                        outcome.total_tokens_freed,
                        estimated_pressure * 100.0,
                    ),
                );
            }
        }
    }

    Ok(PreparedTurnIteration::Ready(TurnIterationPrep {
        quiet,
        turn_start_time,
    }))
}

#[cfg(test)]
mod tests {
    use astra_services::session_journal::ToolCallRecord;
    use serde_json::json;

    use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
    use crate::turn::agentic_loop::host::tests::{MockHost, make_state, text_result};

    use super::*;

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

    #[test]
    fn open_ended_file_exploration_detection_requires_file_and_loop_signal() {
        assert!(is_open_ended_file_exploration(
            "List files, read one, list again, then read again. Keep going."
        ));
        assert!(!is_open_ended_file_exploration(
            "Read README.md and summarize it."
        ));
        assert!(!is_open_ended_file_exploration(
            "Keep going on the implementation plan."
        ));
        assert!(!is_open_ended_file_exploration(
            "Read the failing tests, keep going with the refactor until they pass."
        ));
        assert!(!is_open_ended_file_exploration(
            "Read the design doc again, then write the implementation."
        ));
    }

    #[test]
    fn open_ended_file_exploration_budget_caps_turns_and_tools() {
        let mut state = make_state();
        state.message =
            "List files using bash, read one, list again, then read again. Keep going.".into();
        state.max_turns = 10;
        state.remaining_turns = 10;
        state.max_tools_per_turn = 15;

        assert!(apply_open_ended_exploration_budget(&mut state));

        assert_eq!(state.max_turns, OPEN_ENDED_EXPLORATION_MAX_TURNS);
        assert_eq!(state.remaining_turns, OPEN_ENDED_EXPLORATION_MAX_TURNS);
        assert_eq!(
            state.max_tools_per_turn,
            OPEN_ENDED_EXPLORATION_MAX_TOOLS_PER_TURN
        );
        // Post-Task #45: exploration-budget message goes into the
        // structured volatile lane, not state.messages. The singleton
        // dedup in `push_volatile` enforces idempotence.
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|inj| inj.content.contains("Open-ended file exploration budget")),
            "expected exploration-budget injection in volatile lane; got {:?}",
            state.volatile_pending,
        );

        assert!(apply_open_ended_exploration_budget(&mut state));
        let budget_entries = state
            .volatile_pending
            .iter()
            .filter(|inj| inj.content == OPEN_ENDED_EXPLORATION_BUDGET_MESSAGE)
            .count();
        assert_eq!(
            budget_entries, 1,
            "budget injection must be idempotent (singleton dedup); pending={:?}",
            state.volatile_pending,
        );
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

    /// P1-D: Production code must not use unsafe set_var.
    /// Hook env vars must go through session_env_overlay instead.
    #[test]
    fn no_unsafe_set_var_in_production() {
        let source = include_str!("lifecycle.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            !prod_code.contains("std::env::set_var"),
            "production code must not use std::env::set_var (UB in multi-threaded context); \
             use astra_core::session_env_overlay::set instead"
        );
        assert!(
            prod_code.contains("session_env_overlay::set"),
            "hook env vars must be set via session_env_overlay"
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
            .find(|inj| inj.content.contains("[turn-budget]"))
            .map(|inj| inj.content.as_str())
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
}
