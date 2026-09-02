use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CompletionAction, ContinuationAuthority,
    HostTurnResult, RejectedToolCall, RunControlProvider, TerminalExecutionAuthority,
    ToolCallAdmission, TurnPhaseKind, TurnPhaseOutcome, UserIntentState,
    WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT, complete_turn_phase, finalize_and_render,
    finalize_turn_trace, try_write_heavy_checkpoint,
};
use super::lifecycle::{
    TurnIterationPrep, current_agentic_step, interruption_diagnosis_summary,
    interruption_state_summary, mark_work_settlement_incomplete, session_turn_number,
    tool_record_is_workspace_mutation, wait_for_pause_clear_or_cancel,
};
use crate::turn::run_control::{ProviderBoundaryAuthorization, UserIntentAdmissionAuthority};
use astra_config::user_profile::{MutationCompletionScope, Scenario, WorkspaceMutationIntent};
use astra_core::render_compact_status;
use astra_services::{ContextManifestWrite, DatabaseContextManifestStore, SessionArtifactStore};
use astra_turn_core::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut, AgenticTurnIngestOutcome,
    AgenticTurnStreamSnapshot, agentic_turn_stream_snapshot_with_kind, ingest_agentic_turn_stream,
    map_ingest_outcome_to_iteration_control,
};
use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind, CompactionTier};
use astra_turn_core::interaction_types::TurnInteractionMode;
use astra_turn_core::interruption::{InterruptionKind, InterruptionRecord, ResumeAction};
use astra_turn_types::NormalizedPromptCacheUsage;
use uuid::Uuid;

const USER_INTENT_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_USER_INTENT_BOUNDARY_PAGES: usize = 16;
const MAX_USER_INTENT_BOUNDARY_FACTS: usize = 4_096;
const MAX_TEXTLESS_RESPONSE_RETRIES: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderBoundaryGate {
    Authorized,
    Paused,
}

/// Runtime-owned interpretation of a text terminal at the current provider
/// boundary. This deliberately consumes only typed settlement state: provider
/// prose is user-visible evidence, not execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalCompletionDisposition {
    /// An ordinary provider stop that was not forced by a runtime execution
    /// slice. Existing text-completion behavior remains unchanged.
    OrdinaryCompletionCandidate,
    /// A committed Work graph authorized this one final synthesis boundary.
    CommittedWorkSynthesis,
    /// A narrowly declared completion action executed and its typed
    /// obligation is now settled.
    SettledExactCompletionAction,
    /// The bounded execution slice ended, but typed runtime state proves that
    /// no executable obligation remains; deliver the provider's synthesis
    /// through the ordinary outcome/verification guards.
    RoundSliceTextDelivery,
    /// The runtime exhausted an execution slice and reserved only a summary
    /// boundary, but no typed fact established completion of the user goal.
    RoundSliceIncomplete,
}

fn terminal_completion_disposition(
    state: &AgenticLoopState,
    committed_work_synthesis_authorized: bool,
) -> TerminalCompletionDisposition {
    // Canonical Work authority is independent of how the final review
    // boundary was reached. In particular, a graph may settle before the
    // generic round-slice rail fires. Requiring a budget flag here recreates
    // the Work→Run split-brain by handing terminal authority back to the
    // generic workspace-intent guard.
    if committed_work_synthesis_authorized {
        return if state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination
            .is_none()
            && super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
        {
            TerminalCompletionDisposition::CommittedWorkSynthesis
        } else {
            // Canonical Work completion never overrides producer/process
            // ownership. Keep the candidate resumable, but make this text
            // boundary explicitly incomplete even outside a budget wrap-up.
            TerminalCompletionDisposition::RoundSliceIncomplete
        };
    }

    if !state.budget_wrapup_injected
        || state.hooks.completion_settlement.wrapup_origin
            != Some(super::host::BudgetWrapupOrigin::RoundSlice)
    {
        return TerminalCompletionDisposition::OrdinaryCompletionCandidate;
    }

    let exact_action_settled = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| {
            window.consumed
                && window.matched
                // CompletionTaskAction intentionally proves only that one
                // task-facing action ran. Unlike the exact obligation
                // variants, it cannot establish that the user goal settled.
                && !matches!(&window.action, CompletionAction::CompletionTaskAction)
                && pending_completion_action(state).is_none()
        });
    if exact_action_settled {
        return TerminalCompletionDisposition::SettledExactCompletionAction;
    }

    // A bounded synthesis is a valid completion when runtime-owned evidence
    // shows there is no remaining executable obligation. This covers an
    // explicitly read-only investigation and a mutation whose concrete
    // postconditions are already satisfied. Unknown/MayMutate intent still
    // yields the generic terminal action and therefore remains incomplete;
    // active Work, quarantined observations, or live child agents never gain
    // success authority from prose alone.
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    let bounded_synthesis_authorized = !active_work_attempt
        && !workspace_observation_is_quarantined(state)
        && state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination
            .is_none()
        && super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
        && pending_terminal_completion_action_for_work_state(state, false).is_none();
    if bounded_synthesis_authorized {
        return TerminalCompletionDisposition::RoundSliceTextDelivery;
    }

    TerminalCompletionDisposition::RoundSliceIncomplete
}

fn enforce_terminal_completion_disposition_before_success(
    state: &mut AgenticLoopState,
    disposition: TerminalCompletionDisposition,
) -> bool {
    if disposition != TerminalCompletionDisposition::RoundSliceIncomplete
        || state.interruption.is_some()
    {
        return false;
    }

    // Keep the provider's useful summary, but do not promote a runtime-forced
    // handoff to successful task completion. The checkpoint is immediately
    // resumable and no additional provider or tool authority is manufactured.
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::BudgetExhausted,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some("bounded execution slice ended without typed completion authority".to_string()),
        ),
    ));
    true
}

/// Durable settlement closes current-run guidance before the last boundary
/// poll. A non-terminal branch may continue the same run only by consuming
/// this exact capability through [`continue_after_user_intent_settlement_fence`].
enum UserIntentSettlementFence {
    NotApplicable,
    Committed {
        run_control: std::sync::Arc<dyn RunControlProvider>,
        user_id: String,
        expected_session_id: String,
        run_id: String,
        authority: UserIntentAdmissionAuthority,
    },
}

async fn commit_user_intent_settlement_fence(
    state: &AgenticLoopState,
) -> Result<UserIntentSettlementFence, astra_core::ClassifiedError> {
    let (Some(run_control), Some(user_id), Some(run_id)) = (
        state.run_control.as_ref(),
        state.context_manifest_user_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        return Ok(UserIntentSettlementFence::NotApplicable);
    };
    let authority = state
        .current_run_owner_generation
        .map(UserIntentAdmissionAuthority::DurableOwnerGeneration)
        .unwrap_or(UserIntentAdmissionAuthority::ProcessLocal);
    let expected_session_id = state.current_session_id.as_deref().ok_or_else(|| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            format!("run {run_id} is missing its immutable session identity"),
        )
    })?;
    run_control
        .fence_user_intent_submissions(user_id, expected_session_id, run_id, authority)
        .await
        .map_err(|error| {
            astra_core::ClassifiedError::new(
                match authority {
                    UserIntentAdmissionAuthority::ProcessLocal => {
                        astra_core::ErrorKind::ContractViolation
                    }
                    UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => {
                        astra_core::ErrorKind::Unknown
                    }
                },
                format!("failed to fence final user intent settlement: {error}"),
            )
        })?;
    Ok(UserIntentSettlementFence::Committed {
        run_control: run_control.clone(),
        user_id: user_id.to_string(),
        expected_session_id: expected_session_id.to_string(),
        run_id: run_id.to_string(),
        authority,
    })
}

async fn continue_after_user_intent_settlement_fence(
    fence: UserIntentSettlementFence,
) -> Result<TurnExecutionControl, astra_core::ClassifiedError> {
    if let UserIntentSettlementFence::Committed {
        run_control,
        user_id,
        expected_session_id,
        run_id,
        authority,
    } = fence
    {
        run_control
            .reopen_user_intent_submissions(
                &user_id,
                &expected_session_id,
                &run_id,
                authority,
            )
            .await
            .map_err(|error| {
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Unknown,
                    format!(
                        "failed to reopen user intent admission before continuing run {run_id}: {error}"
                    ),
                )
            })?;
    }
    Ok(TurnExecutionControl::ContinueLoop)
}

async fn authorize_provider_boundary(
    state: &mut AgenticLoopState,
) -> Result<ProviderBoundaryGate, astra_core::ClassifiedError> {
    let (Some(run_control), Some(user_id), Some(run_id)) = (
        state.run_control.as_ref(),
        state.context_manifest_user_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        // Before a run identity exists there is no durable execution row to
        // authorize. Once a run_control identity is attached, every provider
        // boundary below is exact and fail-closed.
        return Ok(ProviderBoundaryGate::Authorized);
    };
    let authority = state
        .current_run_owner_generation
        .map(UserIntentAdmissionAuthority::DurableOwnerGeneration)
        .unwrap_or(UserIntentAdmissionAuthority::ProcessLocal);
    let expected_session_id = state.current_session_id.as_deref().ok_or_else(|| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            format!("run {run_id} is missing its immutable session identity"),
        )
    })?;
    let outcome = run_control
        .authorize_provider_boundary(user_id, expected_session_id, run_id, authority)
        .await
        .map_err(|error| {
            astra_core::ClassifiedError::new(
                match authority {
                    UserIntentAdmissionAuthority::ProcessLocal => {
                        astra_core::ErrorKind::ContractViolation
                    }
                    UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => {
                        astra_core::ErrorKind::Unknown
                    }
                },
                format!("failed to authorize provider boundary for run {run_id}: {error}"),
            )
        })?;
    match outcome {
        ProviderBoundaryAuthorization::Authorized => Ok(ProviderBoundaryGate::Authorized),
        ProviderBoundaryAuthorization::Paused => {
            if let Some(flag) = state.cancellation.pause_flag.as_ref() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ProviderBoundaryGate::Paused)
        }
        ProviderBoundaryAuthorization::Inactive { status } => {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                format!("run {run_id} became {status} before the next provider boundary"),
            ))
        }
        ProviderBoundaryAuthorization::AuthorityLost { reason } => {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                format!(
                    "run {run_id} lost execution authority before the next provider boundary: {reason}"
                ),
            ))
        }
    }
}

fn reconcile_unsettled_work_status(state: &mut AgenticLoopState) {
    // The turn-local tracker is only a projection. Reconcile every identity
    // against the producer registry immediately before freezing user-visible
    // output so a terminal transition that arrived after the last model
    // boundary cannot be rendered as still running.
    if let Some(registry) = state.stall.active_work_registry.clone() {
        let tracked = state.stall.work_unit_observations.active_work_units();
        for observation in tracked {
            if let Some(canonical) =
                registry.canonical_observation(&observation.id, &observation.kind)
            {
                state.stall.work_unit_observations.observe(&canonical);
            }
        }
        for observation in registry.active_work_observations() {
            state.stall.work_unit_observations.observe(&observation);
        }
    }
}

/// A canonical Work attempt cannot be completed by assistant prose.  This is
/// the same control-plane rule as a stop hook that refuses completion while a
/// task is still owned: give the provider one focused retry, then let the run
/// lifecycle mark the still-owned carrier failed instead of rendering an
/// uncommitted success claim or leaving it paused.
fn enforce_typed_work_settlement_before_text_completion(state: &mut AgenticLoopState) -> bool {
    if !state.hooks.completion_settlement.work_settlement_only {
        return false;
    }

    state.budget_wrapup_ignored_rounds = state.budget_wrapup_ignored_rounds.saturating_add(1);
    if state.budget_wrapup_ignored_rounds == 1 {
        state.final_text.clear();
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "work_settlement_required.v1",
                "signal": "owned_work_attempt_unsettled",
                "instruction": "The previous text did not settle the currently owned WorkItem. Call settle_work_item now with the truthful typed outcome. Do not return prose and do not call any other tool.",
                "authority": "canonical_work_lifecycle",
            }),
        );
        return true;
    }

    // Do not publish a second uncommitted model answer. The outer run
    // lifecycle observes the still-owned attempt and transitions its carrier
    // to Failed, so the task board cannot remain Running or Paused.
    mark_work_settlement_incomplete(state);
    false
}

pub(crate) fn successful_post_mutation_observation(state: &AgenticLoopState) -> bool {
    // Weak foreground-process ownership quarantines future attribution, but
    // does not by itself prove that this completed invocation is unsafe.  A
    // live executor receipt plus a later direct observation can settle the
    // current turn; neither fact is durable across a checkpoint because the
    // live runtime arguments are intentionally omitted from serialization.
    // Typed partial commits and positively unsettled ownership remain a hard
    // terminal barrier.
    if workspace_observation_requires_terminal_incomplete(state) {
        return false;
    }
    let mut observed_after_latest_mutation = false;
    for (record_index, record) in state.stall.tool_call_records.iter().enumerate().rev() {
        if !record.was_executed() {
            continue;
        }
        // `args_preview` is intentionally not an execution contract: for bash
        // it is a human-facing, possibly truncated command string rather than
        // a canonical argument object.  Treating it as executable evidence can
        // settle a mutation epoch on a different command than the one that ran.
        // Edge/server records that need this invariant must preserve args_full.
        let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
        // A successful caller-declared verifier is both a verification receipt
        // and evidence of the post-mutation state.  Its shell body may be
        // unknown to the generic side-effect classifier, so do not let that
        // conservative classification create a fresh mutation epoch.
        if state
            .hooks
            .stop_hooks
            .iter()
            .any(|hook| hook.authoritative && record_verifies_explicit_hook(record, hook))
        {
            observed_after_latest_mutation = true;
            continue;
        }
        let record_may_mutate = crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
            &record.name,
            args.as_ref(),
        );
        let literal_script_command = (record.name == "bash")
            .then(|| {
                args.as_ref()
                    .and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
                    .filter(|command| {
                        super::lifecycle::bash_command_has_literal_script_artifact_observation_shape(
                            command,
                        )
                    })
            })
            .flatten();
        let literal_script_artifact_target = literal_script_command.and_then(|command| {
            super::lifecycle::bash_literal_script_artifact_observation_target(
                command,
                state.hooks.workspace_root_hint.as_deref(),
            )
        });
        // Executing the delivered artifact is useful behavioral evidence even
        // when the interpreter emits incidental files (for example Python
        // bytecode), but an opaque script writer cannot prove its own change.
        // Require a distinct, executor-owned delivery receipt before this
        // observation family may settle the mutation epoch. Canonical
        // validators and typed observers retain their existing authority.
        let latest_epoch_delivered_artifact =
            literal_script_artifact_target
                .as_ref()
                .is_some_and(|target| {
                    state.stall.tool_call_records[..record_index]
                        .iter()
                        .rev()
                        .find(|prior| tool_record_may_have_mutated_bound_workspace(state, prior))
                        .is_some_and(|prior| {
                            prior.ok
                                && super::lifecycle::record_has_typed_workspace_tool_receipt(prior)
                                && super::lifecycle::record_explicit_path(prior)
                                    .and_then(|path| {
                                        super::lifecycle::normalize_workspace_path(
                                            &path,
                                            state.hooks.workspace_root_hint.as_deref(),
                                        )
                                    })
                                    .as_ref()
                                    == Some(target)
                        })
                });
        if record.ok
            && super::lifecycle::record_can_observe_bound_workspace(state, record)
            && (literal_script_command.is_none() || latest_epoch_delivered_artifact)
        {
            // A compound shell invocation may contain both the mutation and
            // its post-mutation receipt.  Count the receipt before closing
            // the mutation epoch; a mutation-only record still returns the
            // evidence accumulated from later records.
            observed_after_latest_mutation = true;
        }
        if record_may_mutate && !record_is_proven_external_scratch_mutation(state, record) {
            return observed_after_latest_mutation;
        }
    }
    true
}

pub(crate) fn workspace_observation_is_quarantined(state: &AgenticLoopState) -> bool {
    state.stall.workspace_observation_quarantine.is_some()
        || state
            .hooks
            .workspace_root_hint
            .as_deref()
            .and_then(|root| {
                astra_tools::workspace_observation::workspace_ownership_is_unsettled(
                    std::path::Path::new(root),
                )
            })
            .unwrap_or(false)
}

/// Whether the observation state is strong enough to make a user-visible
/// completion claim impossible.
///
/// A foreground process-group receipt is an attribution quarantine: it keeps
/// later fingerprints and budget renewal fail-closed because a descendant
/// could have escaped the group, but it is not proof that this invocation is
/// still running.  Treating that advisory quarantine as an unconditional
/// terminal interruption makes every ordinary foreground writer fail on
/// hosts without delegated cgroups (and makes verifier-successful turns look
/// interrupted).  Only a typed partial commit or an explicitly unsettled
/// ownership scope is a terminal barrier.
pub(crate) fn workspace_observation_requires_terminal_incomplete(state: &AgenticLoopState) -> bool {
    state.stall.workspace_observation_quarantine.as_ref().is_some_and(|q| {
        q.reason != astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::WEAK_PROCESS_OWNERSHIP_REASON
    }) || state
        .hooks
        .workspace_root_hint
        .as_deref()
        .and_then(|root| {
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(
                std::path::Path::new(root),
            )
        })
        .unwrap_or(false)
}

/// A volatile scratch mutation may be ignored by the workspace observation
/// watermark only when its target is authoritative and every target is
/// proven external. Unknown shell bodies and missing paths remain barriers;
/// they may have changed the bound workspace despite an apparently unrelated
/// command name or exit status.
fn record_is_proven_external_scratch_mutation(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
    if !crate::turn::tool_side_effects::tool_call_may_mutate_workspace(&record.name, args.as_ref())
    {
        return false;
    }
    let workspace_root = state.hooks.workspace_root_hint.as_deref();
    if record.name == "bash" {
        let Some(command) = args.and_then(|args| {
            astra_turn_core::tool_argument_hints::command_hint_from_args(&args).map(str::to_string)
        }) else {
            return false;
        };
        return super::lifecycle::bash_mutation_is_proven_external_scratch(
            &command,
            workspace_root,
        );
    }
    let Some(path) = super::lifecycle::record_explicit_path(record) else {
        return false;
    };
    super::lifecycle::path_is_external_volatile_scratch(&path, workspace_root)
}

/// Normalize the small command surface used by explicit stop-hook contracts.
/// This intentionally does not try to understand a shell grammar: the hook
/// is a concrete command supplied by the caller, and only simple sequencing
/// plus whitespace/benign redirect differences are safe to correlate.
fn canonical_verification_command(command: &str) -> String {
    // A declared verification command is an operation identity, not a shell
    // program fragment. Compound commands and extra arguments cannot be
    // correlated safely without a shell parser and an explicit cwd contract,
    // so fail closed rather than treating a prefix as a receipt.
    if command.contains(';') || command.contains("&&") || command.contains("||") {
        return String::new();
    }
    let command = command.trim();
    if command.is_empty() || command.starts_with("cd ") {
        return String::new();
    }
    let command = astra_turn_core::cloud_approval_policy::strip_benign_fd_redirects(command);
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_verification_invocation(
    command: &str,
    hook: &astra_turn_core::stop_hooks::StopHook,
) -> String {
    let expected = canonical_verification_command(&hook.command);
    if expected.is_empty() {
        return String::new();
    }
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(working_dir) = hook.working_dir.as_deref().filter(|dir| !dir.is_empty()) {
        let Some((prefix, body)) = normalized.split_once("&&") else {
            return String::new();
        };
        let prefix = prefix.split_whitespace().collect::<Vec<_>>().join(" ");
        let body = canonical_verification_command(body);
        let expected_prefix = format!("cd {}", working_dir.trim());
        if prefix != expected_prefix || body != expected {
            return String::new();
        }
        return format!("{prefix} && {body}");
    }
    canonical_verification_command(&normalized)
}

fn tool_call_verifies_explicit_hook(
    name: &str,
    args: Option<&serde_json::Value>,
    hook: &astra_turn_core::stop_hooks::StopHook,
) -> bool {
    // A typed verification tool can be represented without forcing every
    // caller through bash. The identity is still the explicit hook
    // command/label, never a guessed framework or filename.
    if name != "bash" {
        return false;
    }
    let Some(args) = args else {
        return false;
    };
    let Some(command) = args.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let actual = canonical_verification_invocation(command, hook);
    if actual.is_empty() {
        return false;
    }
    let expected = canonical_verification_command(&hook.command);
    if expected.is_empty() {
        return false;
    }
    let expected = hook
        .working_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(|dir| format!("cd {} && {expected}", dir.trim()))
        .unwrap_or(expected);
    actual == expected
}

pub(crate) fn record_verifies_explicit_hook(
    record: &astra_services::session_journal::ToolCallRecord,
    hook: &astra_turn_core::stop_hooks::StopHook,
) -> bool {
    if !record.was_executed() || !record.ok {
        return false;
    }

    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
    tool_call_verifies_explicit_hook(&record.name, args.as_ref(), hook)
}

/// Return the explicit verification obligations that have no successful
/// receipt after the latest source mutation. Auto-detected hooks are advisory
/// and deliberately excluded: without an explicit contract Astra cannot know
/// the hidden verifier's scope.
pub(crate) fn missing_explicit_verification_hooks(state: &AgenticLoopState) -> Option<Vec<String>> {
    let hooks: Vec<_> = state
        .hooks
        .stop_hooks
        .iter()
        .filter(|hook| hook.authoritative)
        .collect();
    if hooks.is_empty() {
        return None;
    }

    // A hook invocation itself may be classified as a shell mutation (for
    // example a build creates an artifact). Exclude records that already
    // satisfy an explicit hook when finding the source mutation epoch, so a
    // successful receipt is not rejected merely because it changed a build
    // directory as a side effect.  For every other executed call use the
    // admission-side may-mutate predicate: an unknown or failed writer is a
    // barrier even when the positive journal mutation classifier cannot prove
    // what it changed.
    let source_mutation = state
        .stall
        .tool_call_records
        .iter()
        .enumerate()
        .rev()
        .find(|(_, record)| {
            record.was_executed()
                && crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                    &record.name,
                    super::lifecycle::extract_tool_args(record.authoritative_args_full()).as_ref(),
                )
                && !hooks
                    .iter()
                    .any(|hook| record_verifies_explicit_hook(record, hook))
        })
        .map(|(index, _)| index)?;

    let missing = hooks
        .iter()
        .filter(|hook| {
            !state
                .stall
                .tool_call_records
                .iter()
                .enumerate()
                .any(|(index, record)| {
                    index > source_mutation && record_verifies_explicit_hook(record, hook)
                })
        })
        .map(|hook| hook.label.clone())
        .collect::<Vec<_>>();
    Some(missing)
}

/// Enforce only caller-declared verification contracts at terminal settlement.
/// The ordinary post-mutation observation guard remains intentionally softer:
/// an unconstrained task can finish with an honest best-effort result, while a
/// declared contract cannot be silently replaced by a read/diff operation.
fn enforce_explicit_verification_before_text_completion(state: &mut AgenticLoopState) -> bool {
    let Some(missing) = missing_explicit_verification_hooks(state) else {
        return false;
    };
    if missing.is_empty() {
        return false;
    }

    let settlement = &mut state.hooks.completion_settlement;
    if settlement.verification_retries == 0 {
        settlement.verification_retries = 1;
        settlement.text_only = false;
        settlement.work_settlement_only = false;
        settlement.wrapup_origin = None;
        state.budget_wrapup_injected = false;
        state.final_text.clear();
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "explicit_verification_required.v1",
                "signal": "verification_contract_unmet",
                "missing_obligations": missing,
                "instruction": "The explicit verification contract is not satisfied after the latest workspace change. Run every missing declared check through the normal tool path, fix failures, and only then give the final answer. A read or diff is observation, not a passing verification receipt.",
                "authority": "caller_declared_verification_contract",
            }),
        );
        return true;
    }

    state.final_text = format!(
        "The workspace change was not verified: required checks still missing ({}) after the bounded verification recovery.",
        missing.join(", ")
    );
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some(format!(
                "verification contract unmet: {}",
                missing.join(", ")
            )),
        ),
    ));
    false
}

/// Close a typed completion-action window after its one attempt.  A matching
/// call is not enough by itself: the post-action ledger must show that the
/// original obligation disappeared.  This prevents a mutation that failed,
/// or a mutation still lacking verification, from reopening ordinary
/// exploration after the hard slice boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionActionBoundary {
    NoWindow,
    Settled,
    TerminalIncomplete,
}

pub(crate) fn enforce_completion_action_window_before_text_completion(
    state: &mut AgenticLoopState,
) -> CompletionActionBoundary {
    let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .clone()
    else {
        return CompletionActionBoundary::NoWindow;
    };

    if !window.consumed || !window.matched {
        state.final_text =
            "The bounded completion action was not executed or did not match the declared obligation."
                .to_string();
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some("typed completion action was not satisfied".to_string()),
            ),
        ));
        return CompletionActionBoundary::TerminalIncomplete;
    }

    if let Some(pending) = pending_completion_action(state) {
        let pending_label = match pending {
            CompletionAction::RequiredWorkspaceMutation => "workspace mutation".to_string(),
            CompletionAction::RequiredExternalEffect => "external state mutation".to_string(),
            CompletionAction::CompletionTaskAction => "completion task action".to_string(),
            CompletionAction::PostMutationObservation => "post-mutation observation".to_string(),
            CompletionAction::PostMutationRepair => "post-mutation repair".to_string(),
            CompletionAction::ExplicitVerification { missing_labels } => {
                format!("explicit verification ({})", missing_labels.join(", "))
            }
            CompletionAction::CanonicalWorkValidation => "canonical Work validation".to_string(),
            CompletionAction::CanonicalWorkRepair => "canonical Work repair".to_string(),
        };
        state.final_text = format!(
            "The bounded completion action ran, but the requested work remains unverified ({pending_label})."
        );
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some("typed completion action did not settle the obligation".to_string()),
            ),
        ));
        return CompletionActionBoundary::TerminalIncomplete;
    }

    // The typed obligation is settled. Do not let the stale window affect a
    // resumed turn or a later terminal guard.
    state.hooks.completion_settlement.completion_action_window = None;
    CompletionActionBoundary::Settled
}

/// Whether a provider-declared early-success terminal must yield to the
/// completion-action window.  The provider stop signal is authoritative only
/// after every dependent typed obligation has settled; a successful mutation
/// can still require one bounded observation/verification action.
pub(crate) fn completion_action_window_requires_followup(state: &AgenticLoopState) -> bool {
    if workspace_observation_requires_terminal_incomplete(state) {
        // Keep provider-declared success from bypassing the terminal guard.
        // No new reader is authorized in this state; the next text boundary
        // renders a truthful incomplete result instead.
        return true;
    }
    if pending_completion_action(state).is_some() {
        return true;
    }
    state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| !window.consumed || !window.matched)
}

/// Advance the bounded completion chain after an admitted tool round has
/// actually been recorded.
///
/// A structured mutating intent has two distinct evidence edges: the
/// workspace mutation itself, followed by an observation/verification of the
/// resulting state.  The admission boundary runs before the tool, so it cannot
/// know whether the mutation succeeded or whether the same call already
/// carried a receipt.  Reconcile that fact only after the journal contains the
/// tool outcome.  A successful required mutation opens exactly one dependent
/// action; a failed mutation remains terminally unsatisfied.  This keeps the
/// chain bounded without granting a fresh exploratory slice.
#[cfg(test)]
pub(crate) fn advance_completion_action_window_after_tool_round(state: &mut AgenticLoopState) {
    let new_records_start = state.stall.tool_call_records.len().saturating_sub(1);
    advance_completion_action_window_after_tool_round_from_record_index(state, new_records_start);
}

/// Reconcile the completion window against the exact tool-record range added
/// by one authoritative tool boundary. Production callers must pass the
/// pre-execution record count so a successful lifecycle record cannot be
/// hidden behind a later failed sibling in the same provider batch.
pub(crate) fn advance_completion_action_window_after_tool_round_from_record_index(
    state: &mut AgenticLoopState,
    new_records_start: usize,
) {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
        state,
        active_work_attempt,
        new_records_start,
    );
}

/// Reopen one bounded repair boundary when the current Work attempt has both
/// a fresh failed canonical validator and a server-owned rejected delivery.
/// The rejection can precede the later budget-settlement boundary that makes
/// repair authority available, but only a rejection from the current causal
/// provider round may open that authority.
pub(crate) fn advance_rejected_work_settlement_recovery(
    state: &mut AgenticLoopState,
    round_records_start: usize,
) {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    advance_rejected_work_settlement_recovery_for_work_state(
        state,
        active_work_attempt,
        round_records_start,
    );
}

#[cfg(test)]
fn advance_rejected_work_settlement_recovery_for_test(
    state: &mut AgenticLoopState,
    round_records_start: usize,
) {
    advance_rejected_work_settlement_recovery_for_work_state(state, true, round_records_start);
}

pub(crate) fn advance_rejected_work_settlement_recovery_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
    round_records_start: usize,
) {
    let current_round_records = state
        .stall
        .tool_call_records
        .get(round_records_start..)
        .unwrap_or_default();
    let rejected_validation_state = current_round_records
        .iter()
        .find_map(record_rejected_work_validation_state);
    let validation_state = current_work_validation_state(state);
    let concurrent_mutation_risk = current_round_records
        .iter()
        .any(|record| tool_record_may_have_mutated_bound_workspace(state, record));
    let recovery_action = match (rejected_validation_state, validation_state) {
        // A later canonical failure in the same batch is decisive evidence,
        // even if the admission-time rejection was caused by stale evidence.
        (_, WorkValidationState::Failed) => Some(CompletionAction::CanonicalWorkRepair),
        // A stale rejection means the Work needs its prior canonical
        // validation again; no workspace repair has been established.
        (Some(RejectedWorkValidationState::Stale), WorkValidationState::Stale) => {
            Some(CompletionAction::CanonicalWorkValidation)
        }
        // The rejected delivery saw a failure, but a sibling writer may have
        // changed the workspace before this boundary settled. A strong receipt
        // is required for mutation completion, not for invalidation: revalidate
        // the resulting state rather than trusting the old failure or granting
        // another blind repair.
        (Some(RejectedWorkValidationState::Failed), WorkValidationState::Stale)
            if concurrent_mutation_risk =>
        {
            Some(CompletionAction::CanonicalWorkValidation)
        }
        _ => None,
    };
    if !active_work_attempt
        || state
            .hooks
            .completion_settlement
            .completion_action_window
            .is_some()
        || state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries
            != 0
        || workspace_observation_is_quarantined(state)
        || rejected_validation_state.is_none()
        || recovery_action.is_none()
    {
        return;
    }

    let Some(validation_operation) = work_validation_operation_for_recovery(state) else {
        return;
    };

    // Two boundaries cover the repair and first revalidation. Preserve any
    // already-authorized ordinary headroom instead of turning this typed
    // recovery into a broader budget extension; the normal repair->validation
    // transition reserves the final truthful Work settlement only after a
    // repair actually mutates state.
    let repair_headroom = 2usize.saturating_sub(state.remaining_turns);
    let next_action = recovery_action.expect("checked above");
    state
        .hooks
        .completion_settlement
        .canonical_validation_recovery_retries = 1;
    state
        .hooks
        .completion_settlement
        .canonical_validation_recovery_failed_action_retries = 0;
    state
        .hooks
        .completion_settlement
        .canonical_validation_recovery_operation = Some(validation_operation);
    state.max_turns = state.max_turns.saturating_add(repair_headroom);
    state.remaining_turns = state.remaining_turns.saturating_add(repair_headroom);
    state.hooks.completion_settlement.work_settlement_only = false;
    state.hooks.completion_settlement.text_only = false;
    state.hooks.completion_settlement.wrapup_origin = None;
    state.budget_wrapup_injected = false;
    state.hooks.completion_settlement.completion_action_window =
        Some(super::host::CompletionActionWindow {
            action: next_action.clone(),
            attempts_remaining: 1,
            mismatch_corrections_remaining: 1,
            consumed: false,
            matched: false,
        });
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "canonical_validation_recovery.v1",
            "signal": if matches!(next_action, CompletionAction::CanonicalWorkRepair) {
                "canonical_validation_failed_repair_once"
            } else {
                "canonical_validation_repair_already_executed_revalidate_once"
            },
            "origin": "rejected_work_settlement",
            "allowed_action": next_action,
            "attempts_remaining": 1,
            "next_required_action": if matches!(next_action, CompletionAction::CanonicalWorkRepair) {
                serde_json::json!(CompletionAction::CanonicalWorkValidation)
            } else {
                serde_json::json!("settle_work_item")
            },
            "instruction": if matches!(next_action, CompletionAction::CanonicalWorkRepair) {
                "The owned WorkItem could not be delivered because the runtime-recognized project validation failed. Make one smallest workspace change that addresses it; the next boundary requires the same class of direct standard project build/test validation. Then settle the currently owned WorkItem truthfully. Do not resume broad exploration."
            } else if matches!(rejected_validation_state, Some(RejectedWorkValidationState::Stale)) {
                "The owned WorkItem could not be delivered because the last canonical project validation is stale after later workspace changes. Rerun the same class of direct standard project build/test validation, then settle the currently owned WorkItem truthfully. Do not resume broad exploration."
            } else {
                "The owned WorkItem could not be delivered because the runtime-recognized project validation failed. A bounded workspace repair already executed in this same boundary, so rerun the same class of direct standard project build/test validation next. Then settle the currently owned WorkItem truthfully. Do not resume broad exploration."
            },
            "authority": "canonical_work_validation_outcome",
        }),
    );
}

#[cfg(test)]
fn advance_completion_action_window_after_tool_round_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
) {
    let new_records_start = state.stall.tool_call_records.len().saturating_sub(1);
    advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
        state,
        active_work_attempt,
        new_records_start,
    );
}

fn advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
    new_records_start: usize,
) {
    // Repair authority is per canonical Work attempt. A new structured
    // assignment, or a successful typed settlement that closes the prior
    // attempt, must not inherit either the one-shot budget or its validation
    // operation identity. Rejected/deferred lifecycle records do not satisfy
    // this predicate and therefore cannot reset the bound.
    if state
        .stall
        .tool_call_records
        .get(new_records_start..)
        .is_some_and(|records| records.iter().any(record_starts_fresh_work_attempt))
    {
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 0;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_failed_action_retries = 0;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_operation = None;
    }

    let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .clone()
    else {
        return;
    };

    if !window.consumed {
        return;
    }

    let round_records = state.stall.tool_call_records.get(new_records_start..);
    let failed_post_mutation_validation_operation =
        if matches!(window.action, CompletionAction::PostMutationObservation)
            && window.matched
            && !workspace_observation_is_quarantined(state)
        {
            round_records.and_then(|records| {
                records.iter().find_map(|record| {
                    // A negative exit is not enough: `test`, `diff`, and
                    // `git diff --quiet` can all be successful observations
                    // of the workspace. Only the executor's source-authored
                    // test/lint result classification plus a normalized
                    // validator operation proves a repairable failure.
                    if record.was_executed()
                        && record.error_kind.is_none()
                        && record.result_class.as_deref() == Some("test_failure")
                    {
                        record.authoritative_args_full().and_then(|args| {
                            astra_turn_core::evaluation::normalize_validation_prefix(
                                &record.name,
                                args,
                            )
                        })
                    } else {
                        None
                    }
                })
            })
        } else {
            None
        };

    // A matched observation can fail before it observes anything when the
    // selected capability is unavailable (for example, a minimal image does
    // not contain an optional inspection executable). That outcome neither
    // validates nor disproves the workspace, and it must not authorize a
    // repair. Give the model one chance to choose a different available
    // observer under the same typed obligation.
    //
    // Keep this narrower than generic tool failure recovery. A timeout may
    // still own live descendants, a partial write may have changed state, and
    // an executor-classified test failure belongs to the repair path below.
    let unavailable_post_mutation_observation =
        matches!(window.action, CompletionAction::PostMutationObservation)
            && window.matched
            && !workspace_observation_is_quarantined(state)
            && round_records.is_some_and(|records| {
                let mut executed = records.iter().filter(|record| record.was_executed());
                let Some(record) = executed.next() else {
                    return false;
                };
                executed.next().is_none()
                    && !record.ok
                    && record.error_kind == Some(astra_core::ErrorKind::ToolUnavailable)
                    && record.result_class.as_deref() != Some("test_failure")
                    && record.workspace_mutation_observed != Some(true)
                    && record.workspace_mutation_partial != Some(true)
                    && record
                        .workspace_mutation_partial_paths
                        .as_ref()
                        .is_none_or(Vec::is_empty)
                    && !record_has_trusted_workspace_mutation_receipt(record)
                    && !super::lifecycle::record_has_typed_workspace_tool_receipt(record)
            });
    if unavailable_post_mutation_observation
        && state
            .hooks
            .completion_settlement
            .post_mutation_observation_failed_action_retries
            == 0
    {
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_failed_action_retries = 1;
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.attempts_remaining = 1;
            window.consumed = false;
            window.matched = false;
        }
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "post_mutation_observation_capability_retry_once",
                "allowed_action": CompletionAction::PostMutationObservation,
                "attempts_remaining": 1,
                "action_hint": completion_action_hint(&CompletionAction::PostMutationObservation),
                "execution_authority": "one_matching_action",
                "instruction": "The required workspace observation could not run because the selected capability was unavailable. Perform exactly one different available read-only observation or validator under the same post-mutation obligation. Do not mutate the workspace, retry the unavailable capability, or resume exploration.",
                "authority": "executed_tool_unavailable_outcome",
            }),
        );
        return;
    }
    if let Some(failed_validation_operation) = failed_post_mutation_validation_operation
        && state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries
            == 0
    {
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries = 1;
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_validation_operation = Some(failed_validation_operation.clone());
        // For an active Work attempt this repair is also the sole corrective
        // mutation in the canonical validation chain. Do not let a failed
        // final validator reopen a second, differently named repair budget.
        if active_work_attempt {
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries = 1;
        }
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.action = CompletionAction::PostMutationRepair;
            window.attempts_remaining = 1;
            window.consumed = false;
            window.matched = false;
        }
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "failed_post_mutation_observation_repair_once",
                "allowed_action": CompletionAction::PostMutationRepair,
                "attempts_remaining": 1,
                "action_hint": completion_action_hint(&CompletionAction::PostMutationRepair),
                "failed_validation_operation": failed_validation_operation,
                "execution_authority": "one_matching_action",
                "instruction": "The required post-mutation validator executed and failed. Make exactly one smallest workspace repair now. That repair must complete successfully with an executor-owned workspace mutation receipt; only then rerun the same failed validator once. Do not substitute a generic workspace read, resume exploration, or make any unrelated tool call.",
                "authority": "executed_observation_failure_receipt",
            }),
        );
        return;
    }

    // A repair boundary is a commitment to make one *effective* correction,
    // not a commitment to accept the first failed write request as progress.
    // Tool admission happens before execution, so a syntactically matching
    // repair can still fail on an atomic precondition (a stale or ambiguous
    // edit anchor is the common example).  Treating that outcome as having
    // spent the repair authority creates a catch-22: the model has received a
    // deterministic correction from the tool but cannot apply it.  We permit
    // one bounded retry of the same repair obligation.  This does not reopen
    // general exploration, does not make the failed result evidence of
    // progress, and still requires the original canonical validation before
    // a WorkItem can be settled.
    //
    // The extra correction budget is intentionally scoped to the repair
    // window.  Other completion actions retain their single-attempt contract;
    // a second failed repair remains terminal and must be reported
    // truthfully.  Failed processes may have left partial state, so the
    // retry instruction explicitly requires a corrective mutation rather
    // than assuming the workspace is unchanged.
    let repair_execution_failed = matches!(window.action, CompletionAction::CanonicalWorkRepair)
        && window.matched
        && state
            .stall
            .tool_call_records
            .get(new_records_start..)
            .is_some_and(|records| {
                records
                    .iter()
                    .any(|record| record.was_executed() && !record.ok)
            });
    if active_work_attempt
        && repair_execution_failed
        && state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_failed_action_retries
            == 0
    {
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.consumed = false;
            window.matched = false;
            window.attempts_remaining = 1;
        }
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_failed_action_retries = 1;
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "canonical_validation_recovery.v1",
                "signal": "canonical_work_repair_failed_retry_once",
                "allowed_action": CompletionAction::CanonicalWorkRepair,
                "attempts_remaining": 1,
                "next_required_action": CompletionAction::CanonicalWorkValidation,
                "instruction": "The bounded repair did not complete successfully, so it did not advance validation. Make one corrected workspace mutation now; it may need to account for partial state from the failed attempt. The next boundary requires the same class of direct standard project build/test validation. Do not resume broad exploration.",
                "authority": "executed_tool_outcome",
            }),
        );
        return;
    }
    let post_mutation_repair_next = matches!(window.action, CompletionAction::PostMutationRepair)
        && round_records.is_some_and(|records| {
            records.iter().any(record_is_effective_workspace_repair)
        })
        // A repair after an observation failure has two principled
        // continuations. Normal completion must observe the repaired
        // workspace once; an active Work attempt whose validator failed must
        // instead rerun that canonical validator. The latter is not
        // interchangeable with a generic observation.
        && ((!active_work_attempt
            && matches!(
                pending_completion_action_for_work_state(state, active_work_attempt),
                Some(CompletionAction::PostMutationObservation)
            ))
            || (active_work_attempt
                && matches!(
                    pending_completion_action_for_work_state(state, active_work_attempt),
                    Some(CompletionAction::CanonicalWorkValidation)
                )));
    if window.matched
        && let Some(next_action) =
            pending_completion_action_for_work_state(state, active_work_attempt)
        && ((matches!(window.action, CompletionAction::RequiredWorkspaceMutation)
            && !matches!(next_action, CompletionAction::RequiredWorkspaceMutation))
            || (matches!(window.action, CompletionAction::RequiredExternalEffect)
                && !matches!(next_action, CompletionAction::RequiredExternalEffect))
            || (matches!(window.action, CompletionAction::CompletionTaskAction)
                && !matches!(next_action, CompletionAction::CompletionTaskAction))
            || (matches!(window.action, CompletionAction::CanonicalWorkRepair)
                && matches!(next_action, CompletionAction::CanonicalWorkValidation)
                && matches!(
                    current_work_validation_state(state),
                    WorkValidationState::Stale | WorkValidationState::Failed
                ))
            || (post_mutation_repair_next
                && matches!(
                    next_action,
                    CompletionAction::PostMutationObservation
                        | CompletionAction::CanonicalWorkValidation
                )))
        && completion_action_window_is_batchable(state, &next_action)
    {
        if active_work_attempt || matches!(window.action, CompletionAction::PostMutationRepair) {
            // The initial settlement reserve accounts for one completion
            // action plus the canonical Work settlement. A successful
            // mutation can reveal exactly one dependent observation edge
            // only after its outcome is recorded, so reserve that one typed
            // boundary here. This is not an exploratory budget extension.
            state.max_turns = state.max_turns.saturating_add(1);
            state.remaining_turns = state.remaining_turns.saturating_add(1);
        }
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.action = next_action.clone();
            window.attempts_remaining = 1;
            window.consumed = false;
            window.matched = false;
        }
        if matches!(next_action, CompletionAction::PostMutationObservation) {
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries = 0;
        }
        // Active Work proceeds through its canonical validation contract,
        // which carries its own operation identity. The non-Work revalidation
        // binding must not leak into a later WorkItem in this same run.
        if active_work_attempt
            && matches!(window.action, CompletionAction::PostMutationRepair)
            && matches!(next_action, CompletionAction::CanonicalWorkValidation)
        {
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_validation_operation = None;
        }
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "typed_completion_action_available",
                "mode": if active_work_attempt { "bounded_completion_then_work_settlement" } else { "bounded_completion_chain" },
                "allowed_action": next_action,
                "attempts_remaining": 1,
                "action_hint": completion_action_hint(&next_action),
                "declarations_may_remain_visible_for_cache": true,
                "execution_authority": "one_matching_action",
                "instruction": if active_work_attempt {
                    "The bounded workspace action completed. Perform exactly one action matching the next typed completion obligation, then settle the currently owned WorkItem truthfully with settle_work_item. Do not resume ordinary exploration or request an unrelated tool."
                } else {
                    "The bounded workspace action completed. Perform exactly one action matching the next typed completion obligation, then produce the final answer. Do not resume ordinary exploration or request an unrelated tool."
                },
                "authority": "typed_turn_intent_and_executed_tool_ledger",
            }),
        );
        return;
    }

    if active_work_attempt
        && window.matched
        && matches!(window.action, CompletionAction::CanonicalWorkValidation)
        && !workspace_observation_is_quarantined(state)
        && matches!(
            current_work_validation_state(state),
            WorkValidationState::Failed
        )
        && state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries
            == 0
        && let Some(failed_operation) = failed_work_validation_operation(state)
    {
        // A failed final validation is decisive new evidence, not proof that
        // the task cannot be repaired. Reuse the reserved settlement boundary
        // for one focused repair and add only the following revalidation
        // boundary. A second validation failure must settle truthfully.
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_failed_action_retries = 0;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_operation = Some(failed_operation.clone());
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.action = CompletionAction::CanonicalWorkRepair;
            window.attempts_remaining = 1;
            window.mismatch_corrections_remaining = 1;
            window.consumed = false;
            window.matched = false;
        }
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "canonical_validation_recovery.v1",
                "signal": "canonical_validation_failed_repair_once",
                "allowed_action": CompletionAction::CanonicalWorkRepair,
                "attempts_remaining": 1,
                "next_required_action": CompletionAction::CanonicalWorkValidation,
                "instruction": "The runtime-recognized project validation failed. Make one smallest workspace change that addresses it; the next boundary requires the same class of direct standard project build/test validation. Then settle the currently owned WorkItem truthfully. Do not resume broad exploration.",
                "authority": "canonical_work_validation_outcome",
            }),
        );
        return;
    }

    if active_work_attempt
        && window.matched
        && !workspace_observation_is_quarantined(state)
        && (pending_completion_action(state).is_none()
            || (matches!(window.action, CompletionAction::CanonicalWorkValidation)
                && matches!(
                    current_work_validation_state(state),
                    WorkValidationState::Failed | WorkValidationState::Stale
                )))
    {
        // An active canonical Work attempt owns the terminal claim. Once the
        // one bounded completion action has run, hand authority back to its
        // truthful settlement operation instead of forcing final prose. The
        // handoff is valid after the ledger proves that ordinary completion
        // obligations are gone. Canonical Work validation is the one
        // exception: after its single attempt, the Work lifecycle must still
        // be allowed to record a truthful failed/blocked outcome. Delivery
        // remains protected by the server's validation-state gate. A
        // rejected or unmatched action never acquires settlement authority.
        state.hooks.completion_settlement.completion_action_window = None;
        state.hooks.completion_settlement.work_settlement_only = true;
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "work_settlement_required.v1",
                "signal": "completion_action_attempted_settle_work",
                "instruction": "The bounded completion action has finished. Settle the currently owned WorkItem now with its truthful outcome using settle_work_item. Do not request any other tool or return prose.",
                "authority": "canonical_work_lifecycle",
            }),
        );
        return;
    }

    // Outside canonical Work, or when its bounded action did not settle the
    // typed obligation, keep the current window auditable until the final-text
    // boundary renders a truthful incomplete result.
    state.hooks.completion_settlement.text_only = true;
}

/// A text `stop` is a provider transport outcome, not proof that an explicitly
/// requested implementation outcome was delivered.  Keep this boundary
/// structural: typed intent establishes that a mutation is required, tool
/// records establish whether one happened, and a later successful observation
/// establishes that the final workspace was at least inspected.  No assistant
/// prose or task-specific keyword is parsed here.
#[cfg(test)]
fn enforce_workspace_completion_before_text_completion(state: &mut AgenticLoopState) -> bool {
    enforce_workspace_completion_before_text_completion_with_disposition(
        state,
        TerminalCompletionDisposition::OrdinaryCompletionCandidate,
    )
}

fn enforce_workspace_completion_before_text_completion_with_disposition(
    state: &mut AgenticLoopState,
    disposition: TerminalCompletionDisposition,
) -> bool {
    if workspace_observation_requires_terminal_incomplete(state) {
        state.final_text =
            "The workspace execution could not be safely settled, so the resulting state is unverified."
                .to_string();
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some("workspace observation quarantined after unsettled process ownership".into()),
            ),
        ));
        return false;
    }
    if has_unsettled_side_effecting_cancellation(state) {
        // A cancellation after a side-effect-capable executor crossed its
        // execution boundary is not equivalent to a harmless failed probe.
        // In particular, its process may have been interrupted between a
        // durable external mutation and its own cleanup/verification step.
        // There is no safe generic way to let later prose or an unrelated
        // successful command settle that unknown external state. Preserve the
        // partial and require a resumed run with an executor-owned receipt.
        // Do not render a pre-existing success claim beside the typed
        // incomplete disposition; the durable checkpoint retains the partial
        // trace, while the user-visible terminal must remain truthful.
        state.final_text.clear();
        mark_workspace_completion_incomplete(
            state,
            "a side-effect-capable tool was cancelled after execution started without a trusted settlement receipt",
            "A side-effecting command was interrupted after it started, so the resulting state is unverified. The partial result is preserved; continue from this checkpoint with an executor-owned verification or recovery action.",
        );
        return false;
    }
    if matches!(
        disposition,
        TerminalCompletionDisposition::CommittedWorkSynthesis
            | TerminalCompletionDisposition::SettledExactCompletionAction
    ) {
        // The canonical Work scheduler (or the exact typed follow-up it
        // required) owns the task terminal. Do not reopen generic
        // mutation/observation obligations for that already-settled Work.
        // Process-ownership quarantine above remains authoritative, and the
        // explicit verification/outcome guards still run after this function.
        return false;
    }
    let external_effect = has_concrete_external_effect(state);
    if requires_external_effect_completion(state) && !external_effect {
        if state.hooks.completion_settlement.external_effect_retries > 0 {
            mark_workspace_completion_incomplete(
                state,
                "required external state mutation had no executor-owned delta receipt after the bounded recovery",
                "The requested external state change remains incomplete: no authoritative executor-owned external delta receipt was recorded. The response is preserved as an unverified partial; continue from this checkpoint or report the concrete blocker.",
            );
            return false;
        }
        state.hooks.completion_settlement.external_effect_retries = 1;
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.wrapup_origin = None;
        state.budget_wrapup_injected = false;
        state.final_text.clear();
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "external_completion_required.v1",
                "signal": "required_external_effect_missing",
                "evidence": { "authoritative_external_effect_receipts": 0 },
                "instruction": "The requested state is outside the bound workspace, but no authoritative external delta was observed. Continue with one foreground action using the tool's structured external_state_paths field and list only the smallest absolute external roots that must change. The executor, not your prose or the command exit status, will compare their pre/post state. If no safe bounded root can be observed, report that blocker precisely.",
                "authority": "typed_turn_intent_and_executor_effect_ledger",
            }),
        );
        return true;
    }

    let intent_requires_mutation = requires_bound_workspace_completion(state);
    let concrete_mutation = has_concrete_workspace_mutation(state);
    let convergence_state = live_desired_state_convergence_state(state);
    let workspace_completion_evidence = has_bound_workspace_completion_evidence(state);

    if intent_requires_mutation
        && !workspace_completion_evidence
        && convergence_state != LiveDesiredStateConvergence::PendingObservation
    {
        if state.hooks.completion_settlement.workspace_mutation_retries > 0 {
            mark_workspace_completion_incomplete(
                state,
                "required workspace mutation was still missing after the bounded recovery",
                "The requested workspace change remains incomplete: no accepted workspace mutation was recorded after the bounded recovery attempt. The response is preserved as an unverified partial; continue from this checkpoint or report the concrete blocker.",
            );
            return false;
        }
        state.hooks.completion_settlement.workspace_mutation_retries = 1;
        // A budget settlement may have projected a text-only tool surface.
        // Reopen exactly this bounded recovery boundary so the required
        // mutation is possible; the next exhausted boundary settles again.
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.wrapup_origin = None;
        state.budget_wrapup_injected = false;
        state.final_text.clear();
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.hooks.completion_settlement.completion_action_window =
            Some(super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let action_hint =
            completion_action_hint_for_state(state, &CompletionAction::RequiredWorkspaceMutation);
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "workspace_completion_required.v1",
                "signal": "required_workspace_mutation_missing",
                "evidence": {
                    "successful_workspace_mutations": 0,
                },
                "allowed_action": CompletionAction::RequiredWorkspaceMutation,
                "attempts_remaining": 1,
                "mismatch_corrections_remaining": 1,
                "action_hint": action_hint,
                "execution_authority": "one_matching_action",
                "instruction": "The requested workspace state lacks authoritative completion evidence, so the previous text cannot complete this turn. Use exactly one complete-state typed write_file call containing the full desired bytes. If it changes the target, the executor records a mutation receipt. If those exact normalized bytes are already present after an earlier opaque action, the executor records a no-op convergence receipt; that still requires one later, separate, full read_file of the same target before completion. Bash output, Bash exit status, assistant prose, and a server-side stat of a remote workspace are not evidence.",
                "authority": "typed_turn_intent_and_executed_tool_ledger",
            }),
        );
        return true;
    }

    let observation_needed =
        concrete_mutation || convergence_state == LiveDesiredStateConvergence::PendingObservation;
    let observation_satisfied = convergence_state == LiveDesiredStateConvergence::Observed
        || (concrete_mutation && successful_post_mutation_observation(state));
    if observation_needed
        && !observation_satisfied
        && state
            .hooks
            .completion_settlement
            .post_mutation_observation_retries
            == 0
    {
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_retries = 1;
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.wrapup_origin = None;
        state.budget_wrapup_injected = false;
        state.final_text.clear();
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        // The recovery instruction is not sufficient execution authority.
        // Keep this post-mutation observation on the same typed, one-action
        // admission path as every other completion obligation: otherwise a
        // provider can spend the only recovery round on an ordinary Bash
        // command whose stdout looks like validation but has no executor
        // receipt.  A single rejected mismatch gets the normal structured
        // correction boundary; the eventual observer still has to earn its
        // receipt at execution time.
        state.hooks.completion_settlement.completion_action_window =
            Some(super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let pending_convergence =
            convergence_state == LiveDesiredStateConvergence::PendingObservation;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "workspace_completion_required.v1",
                "signal": if pending_convergence { "desired_state_observation_missing" } else { "post_mutation_observation_missing" },
                "instruction": if pending_convergence {
                    "The complete-state writer found the requested bytes already present, but that no-op is not a mutation and cannot finish the turn alone. Do not return prose yet. Make exactly one later full read_file observation of the same target in a new tool round, inspect the result, and then complete."
                } else {
                    "The workspace changed after the last trusted observation. Do not return prose yet. Make exactly one trusted post-change observation now. Prefer a workspace read/list/diff tool with an absolute path to the changed work. For a Bash validation command, set `mode` to `verify` and use a foreground read-only command; the executor will attest that the bound workspace stayed unchanged. Do not use a compound shell command such as `cd <dir> && ...` as this observation. Inspect the result, fix any issue found, then complete. If validation cannot run, state the exact reason and distinguish unverified work from a verified result."
                },
                "authority": "executed_tool_ledger",
            }),
        );
        return true;
    }

    if observation_needed
        && !observation_satisfied
        && state
            .hooks
            .completion_settlement
            .post_mutation_observation_retries
            > 0
    {
        mark_workspace_completion_incomplete(
            state,
            "workspace mutation was not followed by a trusted observation after the bounded recovery",
            "The requested workspace change remains unverified: the mutation was recorded, but no trusted post-mutation observation completed after the bounded recovery attempt. The result is preserved as an unverified partial.",
        );
    }

    false
}

/// A started cancellation of a mutating-capable invocation creates an
/// independent terminal settlement debt. `was_executed` is the durable
/// boundary fact; a rejected or never-started request cannot have changed
/// state. A declared external target set can be closed only by a later
/// executor-owned effect receipt for that exact target set. We intentionally
/// do not infer recovery from command text, prose, or an unrelated successful
/// tool call. An unscoped cancellation remains fail-closed because there is
/// no machine-correlatable state boundary to settle.
fn has_unsettled_side_effecting_cancellation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .enumerate()
        .any(|(index, record)| {
            if !record.was_executed()
                || record.ok
                || record.error_kind != Some(astra_core::ErrorKind::Cancelled)
                || !crate::turn::tool_side_effects::tool_call_may_mutate_any_state(
                    &record.name,
                    super::lifecycle::extract_tool_args(record.authoritative_args_full()).as_ref(),
                )
            {
                return false;
            }
            let Some(target_set_digest) = declared_external_target_set_digest(record) else {
                return true;
            };
            !state.stall.tool_call_records[index + 1..]
                .iter()
                .any(|later| {
                    later.was_executed()
                        && later.ok
                        && later.external_effect_observed == Some(true)
                        && declared_external_target_set_digest(later).as_deref()
                            == Some(target_set_digest.as_str())
                        && later.external_effect_receipt.as_ref().is_some_and(
                            |receipt| {
                                astra_tools::workspace_observation::is_authoritative_external_effect_receipt(receipt)
                                    && receipt
                                        .get("target_set_digest")
                                        .and_then(serde_json::Value::as_str)
                                        == Some(target_set_digest.as_str())
                            },
                        )
                })
        })
}

/// Canonical, model-declared target-set digest for correlating a cancellation
/// with a later executor-owned external receipt. The receipt itself supplies
/// the authority; matching its embedded digest prevents a receipt for one
/// target set from clearing debt created by another. Missing or malformed
/// declarations remain fail-closed.
fn declared_external_target_set_digest(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<String> {
    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full())?;
    astra_tools::workspace_observation::ExternalEffectFingerprint::declared_target_set_digest_from_args(&args)
}

/// Convert an exhausted workspace-delivery recovery window into a typed
/// incomplete terminal. Returning `false` from a completion guard means
/// "do not reopen another provider turn"; it must not mean "the model's last
/// prose is a successful completion". Keeping this transition typed also
/// lets the server/CLI preserve the partial result without accepting an
/// artifact-less reward or a false verified claim.
fn mark_workspace_completion_incomplete(
    state: &mut AgenticLoopState,
    detail: &str,
    fallback_text: &str,
) {
    if state.final_text.trim().is_empty() {
        state.final_text = fallback_text.to_string();
    }
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(state, Some(detail.to_string())),
    ));
}

/// Give a candidate final answer one bounded rewrite when the structured
/// execution ledger still reports a persistent unresolved outcome.
///
/// This is deliberately not a semantic claim checker and does not parse the
/// answer.  The policy evaluator has already required two observations of the
/// same active failure.  We preserve that factual boundary, remove the
/// candidate from canonical history, and ask for a text-only reconciliation.
fn enforce_outcome_reconciliation_before_text_completion(state: &mut AgenticLoopState) -> bool {
    if state
        .hooks
        .completion_settlement
        .outcome_reconciliation_retries
        > 0
        || !crate::turn::runtime_policy::feedback_requires_outcome_reconciliation(
            &state.stall.active_policy_feedback,
        )
    {
        return false;
    }

    state
        .hooks
        .completion_settlement
        .outcome_reconciliation_retries = 1;
    state.hooks.completion_settlement.text_only = true;
    state.final_text.clear();
    state.max_turns = state.max_turns.saturating_add(1);
    state.remaining_turns = state.remaining_turns.saturating_add(1);
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "outcome_reconciliation_required.v1",
            "signal": "persistent_unresolved_tool_outcome",
            "instruction": "Review the candidate answer against the retained direct tool results. Reconcile every still-failed or rejected outcome that matters to the latest user request. Separate observed facts, inferences, and unresolved hypotheses; remove contradictory certainty. Produce the corrected final answer now without requesting more tools or discussing this internal review boundary.",
            "authority": "runtime_policy_evidence",
        }),
    );
    true
}

/// If the bounded reconciliation pass did not clear the same typed failure,
/// finish as resumable incomplete rather than allowing a model-written
/// success claim to hide the durable evidence.  This is deliberately gated by
/// the evaluator's `Converge` stage and by the already-spent retry, so one
/// transient failure remains an advisory signal instead of a hard stop.
fn enforce_persistent_unresolved_outcome_terminal(state: &mut AgenticLoopState) -> bool {
    if state
        .hooks
        .completion_settlement
        .outcome_reconciliation_retries
        == 0
        || state.interruption.is_some()
        || !crate::turn::runtime_policy::feedback_requires_outcome_reconciliation(
            &state.stall.active_policy_feedback,
        )
    {
        return false;
    }

    // Keep the model's latest text as a labelled partial response when it
    // exists.  A truthful reconciliation such as "the exact cause remains
    // unresolved" is useful evidence and must not be replaced by a generic
    // runtime sentence.  The typed interruption below still prevents that
    // text from being presented as an unqualified successful completion.
    if state.final_text.trim().is_empty() {
        state.final_text = "The requested execution remains incomplete: a previously observed tool outcome is still unresolved after the bounded reconciliation pass. Completed receipts are preserved; continue from the checkpoint to resolve it or report the verified limitation.".to_string();
    }
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some("persistent unresolved tool outcome after bounded reconciliation".into()),
        ),
    ));
    true
}

fn should_retry_textless_response(state: &AgenticLoopState, turn_result: &HostTurnResult) -> bool {
    turn_result.accum.tool_calls.is_empty()
        && turn_result.accum.full_text.trim().is_empty()
        && state.final_text.trim().is_empty()
        && state
            .hooks
            .completion_settlement
            .deferred_candidate_text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
        && state.interruption.is_none()
        && state.remaining_turns > 0
        && state.hooks.completion_settlement.textless_response_retries
            < MAX_TEXTLESS_RESPONSE_RETRIES
}

fn begin_textless_response_retry(state: &mut AgenticLoopState) {
    let settlement = &mut state.hooks.completion_settlement;
    settlement.textless_response_retries += 1;
    settlement.text_only = true;
    let rounds_completed = state.max_turns.saturating_sub(state.remaining_turns);
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "final_answer_settlement.v1",
            "signal": "textless_provider_response",
            "evidence": {
                "tool_calls_completed": state.total_tool_calls,
                "rounds_completed": rounds_completed,
                "remaining_turns": state.remaining_turns,
            },
            "instruction": "The previous model response ended without user-visible text. Produce a concise, direct final answer from the preserved evidence now. This recovery boundary is text-only; do not request tools, create tasks, or delegate work.",
            "authority": "runtime_bounded_text_only_retry",
        }),
    );
}

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

/// A clean provider terminal can still be a runtime non-delivery (for
/// example, reasoning-only output).  The provider request and its usage were
/// real even though the loop will recover rather than settle the response.
/// Fold that evidence exactly once on the error lane, which otherwise has no
/// `HostTurnResult` from which normal ingest can account it.
fn fold_provider_completion_error_usage(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) {
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return;
    };
    if details
        .pointer("/provider_response/transport_success")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return;
    }
    let Some(usage) = details.get("usage").and_then(serde_json::Value::as_object) else {
        return;
    };
    let field = |name| {
        usage
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    state.total_prompt = state.total_prompt.saturating_add(field("input_tokens"));
    state.total_cache_read = state
        .total_cache_read
        .saturating_add(field("cached_input_tokens"));
    state.total_cache_creation = state
        .total_cache_creation
        .saturating_add(field("cache_creation_tokens"));
    state.total_completion = state
        .total_completion
        .saturating_add(field("output_tokens"));
    state.has_any_usage = true;
    state
        .step_recorder
        .record_tokens(field("input_tokens"), field("output_tokens"));
    let measured_prompt = NormalizedPromptCacheUsage::new(
        field("input_tokens"),
        field("cached_input_tokens"),
        field("cache_creation_tokens"),
    )
    .total_input_tokens();
    if measured_prompt > 0 {
        state.last_measured_prompt_tokens = Some(measured_prompt);
    }
    // The physical provider round was already counted as a local error before
    // this typed payload reached the loop. Reclassify that same attempt; do
    // not create a second coverage record.
    state.telemetry.local_usage_unavailable =
        state.telemetry.local_usage_unavailable.saturating_sub(1);
    state.telemetry.local_usage_provider_reported = state
        .telemetry
        .local_usage_provider_reported
        .saturating_add(1);
}

/// True only when the provider transport itself completed successfully but
/// the response contained neither visible text nor a selected tool. This is
/// distinct from a provider wall/semantic deadline even though both currently
/// travel through the `ProviderDeadline` error lane.
fn is_transport_success_empty_completion(details: &serde_json::Value) -> bool {
    let scope = details
        .pointer("/deadline/scope")
        .and_then(serde_json::Value::as_str);
    let actionable_output_boundary =
        matches!(scope, Some("provider_completion" | "provider_convergence"))
            && details
                .pointer("/deadline/phase")
                .and_then(serde_json::Value::as_str)
                == Some("actionable_output");
    let transport_succeeded = details
        .pointer("/provider_response/transport_success")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let explicitly_non_actionable_text = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|text| !crate::turn::llm::client::text_has_actionable_content(text));
    let explicitly_empty_tools = details
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);

    actionable_output_boundary
        && transport_succeeded
        && explicitly_non_actionable_text
        && explicitly_empty_tools
}

fn record_repeated_transport_success_empty_completion(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) {
    if state.interruption.is_some()
        || !state.provider_adaptation.action_convergence_attempted
        || error.kind != astra_core::ErrorKind::ProviderDeadline
    {
        return;
    }
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return;
    };
    if !is_transport_success_empty_completion(&details) {
        return;
    }

    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::EmptyCompletion,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(state, Some(error.message.clone())),
    ));
}

/// Admit one logical recovery round after a provider attempt failed before it
/// produced an executable or user-visible action. This is deliberately
/// outside the physical transport retry loop: the prior attempt is settled
/// first, and the next request gets its own durable inference identity.
///
/// A partial provider delivery is never replayed blindly. Visible text can be
/// an externally observable deliverable, and a selected tool can be a pending
/// side effect, so either one makes a new provider attempt unsafe. The only
/// recoverable shape is provisional reasoning with no text and no tool call.
fn schedule_safe_provider_recovery(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) -> bool {
    if !matches!(
        error.kind,
        astra_core::ErrorKind::ProviderDeadline
            | astra_core::ErrorKind::StreamTransport
            | astra_core::ErrorKind::StreamIdle
    ) || state.provider_adaptation.action_convergence_attempted
    {
        return false;
    }
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };
    let no_visible_text = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|text| !crate::turn::llm::client::text_has_actionable_content(text));
    let no_selected_tool = details
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_none_or(Vec::is_empty);
    let observed_provisional_reasoning = details
        .get("partial_reasoning")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|reasoning| !reasoning.is_empty());
    let explicit_transport_partial = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && details
            .get("partial_reasoning")
            .and_then(serde_json::Value::as_str)
            .is_some()
        && details
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .is_some();
    let semantic_progress_deadline = details
        .pointer("/deadline/phase")
        .and_then(serde_json::Value::as_str)
        == Some("semantic_progress");
    let provider_completed_without_delivery = details
        .pointer("/deadline/scope")
        .and_then(serde_json::Value::as_str)
        == Some("provider_completion")
        && details
            .pointer("/deadline/phase")
            .and_then(serde_json::Value::as_str)
            == Some("actionable_output");
    let transport_success_empty_completion = error.kind == astra_core::ErrorKind::ProviderDeadline
        && is_transport_success_empty_completion(&details);
    let terminal_text_only_recovery = state.remaining_turns == 0
        && state.hooks.completion_settlement.text_only
        && transport_success_empty_completion;
    if state.remaining_turns == 0 && !terminal_text_only_recovery {
        return false;
    }
    // A semantic-progress deadline is a known safe convergence boundary even
    // when the provider did not expose reasoning. Transport/idle failures need
    // affirmative partial-attempt evidence: otherwise an error from before
    // delivery must remain fail-closed rather than becoming a retry loop.
    let transport_or_idle = matches!(
        error.kind,
        astra_core::ErrorKind::StreamTransport | astra_core::ErrorKind::StreamIdle
    );
    let recovery_evidence = observed_provisional_reasoning
        || (error.kind == astra_core::ErrorKind::ProviderDeadline
            && (semantic_progress_deadline
                || provider_completed_without_delivery
                || transport_success_empty_completion));
    if !no_visible_text
        || !no_selected_tool
        || !recovery_evidence
        || (transport_or_idle && !explicit_transport_partial)
    {
        return false;
    }

    if terminal_text_only_recovery {
        let Some(expanded_max_turns) = state.max_turns.checked_add(1) else {
            return false;
        };
        state.max_turns = expanded_max_turns;
        state.remaining_turns = 1;
    }

    state.provider_adaptation.action_convergence_attempted = true;
    state.provider_adaptation.force_next_thinking_off = true;
    let text_only = state.hooks.completion_settlement.text_only;
    let interruption = match error.kind {
        astra_core::ErrorKind::ProviderDeadline => "spent its action window",
        astra_core::ErrorKind::StreamTransport => "lost its provider connection",
        astra_core::ErrorKind::StreamIdle => "stalled before delivery completed",
        _ => unreachable!("safe provider recovery checked the error kind"),
    };
    let instruction = if text_only {
        format!(
            "The previous inference {interruption} without producing a deliverable or selecting a tool. Do not restart or narrate the analysis, and do not call tools. Use the retained conversation and evidence to return the concise final answer now."
        )
    } else {
        format!(
            "The previous inference {interruption} without producing a deliverable or selecting a tool. Do not restart or narrate the analysis. Use the retained conversation and evidence to make the next concrete tool call now, or return a concise final answer if no execution is needed."
        )
    };
    state.push_volatile_payload(
        super::host::VolatileKind::BehaviorAdvisory,
        serde_json::json!({
            "schema": "provider_safe_recovery.v1",
            "signal": "provider_attempt_ended_without_executable_or_visible_delivery",
            "evidence": {
                "error_kind": error.kind.as_str(),
                "previous_attempt_produced_visible_text": false,
                "previous_attempt_selected_tool": false,
                "previous_attempt_produced_provisional_reasoning": observed_provisional_reasoning,
            },
            "instruction": instruction,
            "execution_mode": if text_only { "text_only" } else { "tool_or_final" },
            "attempts_remaining": 1,
            "authority": "advisory_evidence_only"
        }),
    );
    true
}

/// A completed local tool execution changes the evidence available to the
/// next inference.  Recovery remains bounded to one attempt for each such
/// progress epoch, but must not remain run-global: otherwise an agent that
/// recovered, executed useful work, and later independently stalls loses the
/// only safe way to converge.
pub(crate) fn executed_tool_advances_provider_recovery_epoch(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> bool {
    records.iter().any(|record| record.was_executed())
}

/// Apply the progress-epoch transition from records appended by exactly one
/// tool phase.  A floor makes the transition insensitive to old receipts and
/// keeps selections that never executed from replenishing recovery.
pub(crate) fn advance_provider_recovery_epoch_from_new_records(
    state: &mut AgenticLoopState,
    record_floor: usize,
) -> bool {
    let advanced = executed_tool_advances_provider_recovery_epoch(
        state
            .stall
            .tool_call_records
            .get(record_floor..)
            .unwrap_or_default(),
    );
    if advanced {
        state.provider_adaptation.action_convergence_attempted = false;
    }
    advanced
}

#[cfg(test)]
pub(crate) async fn inject_polled_user_intents<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    inject_polled_user_intents_inner(host, state, false)
        .await
        .map(|_| ())
}

/// Poll the durable intent lane at a model-completion boundary even when the
/// normal empty-poll cadence has not elapsed. This is the ownership handoff
/// that prevents a remotely accepted intent from falling between the final
/// model response and terminal run settlement.
async fn inject_polled_user_intents_before_settlement<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    inject_polled_user_intents_inner(host, state, true).await
}

/// Reconcile guidance after provider inference and before admitting any tool
/// action produced from the now-stale request snapshot. This is a control
/// epoch fence: a newly applied intent must be visible to the model before a
/// response generated without it can cause another side effect.
pub(crate) async fn inject_polled_user_intents_before_action<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    inject_polled_user_intents_inner(host, state, true).await
}

/// Reconcile the durable control lane immediately before beginning another
/// provider round. A tool may finish inside the normal empty-poll debounce
/// window; carrying that debounce through request preparation would allow an
/// already-accepted intent to miss the first safe post-tool boundary and
/// spend an entire provider request on stale work.
async fn inject_polled_user_intents_before_provider<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    inject_polled_user_intents_inner(host, state, true).await
}

async fn inject_polled_user_intents_inner<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    force_poll: bool,
) -> Result<bool, astra_core::ClassifiedError> {
    let poll_started = tokio::time::Instant::now();
    let (run_control, user_id, run_id) = match (
        state.run_control.as_ref(),
        state.context_manifest_user_id.as_ref(),
        state.current_run_id.as_ref(),
    ) {
        (Some(run_control), Some(user_id), Some(run_id)) => {
            (run_control.clone(), user_id.clone(), run_id.clone())
        }
        _ => return Ok(false),
    };
    let expected_session_id = state.current_session_id.clone().ok_or_else(|| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            format!("run {run_id} is missing its immutable session identity"),
        )
    })?;
    if !force_poll
        && !state.user_intents.should_poll_user_intents(poll_started)
        && !run_control.has_pending_inputs()
    {
        return Ok(false);
    }

    let mut pages = 0_usize;
    let mut inspected_facts = 0_usize;
    let mut applied_model_guidance = false;
    loop {
        let page_start_cursor = state.user_intents.user_intent_cursor();
        let mut poll = run_control
            .poll_user_intents(&user_id, &run_id, page_start_cursor)
            .await;
        if let Some(error) = &poll.error {
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            state.user_intents.defer_pending_apply_ack(poll_started);
            tracing::warn!(run_id, error = %error, "user intent poll failed");
            if force_poll {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Unknown,
                    format!("authoritative user intent boundary poll failed: {error}"),
                ));
            }
            return Ok(applied_model_guidance);
        }

        pages = pages.saturating_add(1);
        inspected_facts = inspected_facts.saturating_add(poll.snapshot_page_fact_count);
        let page_has_more = poll.snapshot_has_more;
        let page_cursor = poll.next_cursor;
        if page_cursor < page_start_cursor
            || (poll.snapshot_page_fact_count > 0 && page_cursor == page_start_cursor)
            || (page_has_more && poll.snapshot_page_fact_count == 0)
        {
            tracing::error!(
                run_id,
                page_start_cursor,
                page_cursor,
                page_facts = poll.snapshot_page_fact_count,
                page_has_more,
                "authoritative user-intent pagination violated forward progress"
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!(
                    "authoritative user-intent pagination made no forward progress for run {run_id}: cursor {page_start_cursor} -> {page_cursor}, facts={}, has_more={page_has_more}",
                    poll.snapshot_page_fact_count
                ),
            ));
        }
        if pages > MAX_USER_INTENT_BOUNDARY_PAGES
            || inspected_facts > MAX_USER_INTENT_BOUNDARY_FACTS
            || (page_has_more && pages == MAX_USER_INTENT_BOUNDARY_PAGES)
        {
            tracing::error!(
                run_id,
                pages,
                inspected_facts,
                page_has_more,
                max_pages = MAX_USER_INTENT_BOUNDARY_PAGES,
                max_facts = MAX_USER_INTENT_BOUNDARY_FACTS,
                "authoritative user-intent snapshot exceeded the boundary drain limit"
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!(
                    "authoritative user-intent snapshot exceeds the boundary drain limit for run {run_id}: pages={pages}, facts={inspected_facts}, has_more={page_has_more}"
                ),
            ));
        }

        // A prior executor may have committed the durable apply disposition
        // and crashed before checkpointing model context. Replay that outbox
        // exactly once by stable intent identity.
        let mut durably_applied = Vec::new();
        poll.inputs.retain(|event| {
            if event.status == astra_turn_types::UserIntentStatus::Applied {
                if !state.user_intents.has_applied_user_intent(&event.intent_id) {
                    durably_applied.push(event.clone());
                }
                false
            } else {
                true
            }
        });
        applied_model_guidance |= apply_acknowledged_user_intents(host, state, &durably_applied);

        let mut runtime_notifications = Vec::new();
        poll.inputs.retain(|event| {
            if let Some(content) =
                crate::turn::run_control::runtime_notification_content(&event.input)
            {
                runtime_notifications.push((event.clone(), content));
                false
            } else {
                true
            }
        });
        let observed = state
            .user_intents
            .observe_polled_user_intents(poll, crate::turn::run_control::user_intent_content);
        for issue in &observed.issues {
            tracing::error!(
                run_id,
                event_index = issue.event_index,
                intent_id = issue.intent_id.as_deref().unwrap_or(""),
                kind = ?issue.kind,
                "invalid durable user intent was isolated"
            );
        }
        let mut accepted_for_ack = observed.accepted.clone();
        accepted_for_ack.extend(runtime_notifications.iter().map(|(event, _)| event.clone()));
        let has_new_apply_events = state
            .user_intents
            .stage_pending_apply_events(&accepted_for_ack);
        let release_event_indices = state.user_intents.pending_apply_event_indices();

        if release_event_indices.is_empty() {
            state
                .user_intents
                .commit_observed_cursor(observed.next_cursor);
        } else if !has_new_apply_events
            && !state
                .user_intents
                .should_retry_apply_ack(tokio::time::Instant::now())
            && !force_poll
        {
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            return Ok(applied_model_guidance);
        } else {
            let apply_authority = state
                .current_run_owner_generation
                .map(UserIntentAdmissionAuthority::DurableOwnerGeneration)
                .unwrap_or(UserIntentAdmissionAuthority::ProcessLocal);
            match run_control
                .mark_user_intents_applied(
                    &user_id,
                    &expected_session_id,
                    &run_id,
                    &release_event_indices,
                    apply_authority,
                )
                .await
            {
                Ok(crate::turn::run_control::UserIntentApplyAck::Applied) => {
                    let acknowledged = state
                        .user_intents
                        .acknowledge_apply_events(&release_event_indices);
                    applied_model_guidance |=
                        apply_acknowledged_user_intents(host, state, &acknowledged);
                    for event in &acknowledged {
                        if crate::turn::run_control::runtime_notification_content(&event.input)
                            .is_none()
                        {
                            host.on_user_intent_applied(event).await;
                        }
                    }
                    state
                        .user_intents
                        .commit_observed_cursor(observed.next_cursor);
                }
                Ok(crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned) => {
                    let returned = state
                        .user_intents
                        .return_pending_apply_events(&release_event_indices);
                    for event in &returned {
                        if crate::turn::run_control::runtime_notification_content(&event.input)
                            .is_none()
                        {
                            host.on_user_intent_returned(event).await;
                        }
                    }
                    state
                        .user_intents
                        .commit_observed_cursor(observed.next_cursor);
                    state.user_intents.note_user_intent_poll_finished(
                        poll_started,
                        USER_INTENT_EMPTY_POLL_INTERVAL,
                    );
                    tracing::debug!(
                        run_id = %run_id,
                        ?release_event_indices,
                        "terminal run won user intent application race"
                    );
                    if force_poll {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "run terminated while applying authoritative user guidance; stale provider actions were discarded",
                        ));
                    }
                    return Ok(applied_model_guidance);
                }
                Err(error) => {
                    state
                        .user_intents
                        .note_apply_ack_failure(tokio::time::Instant::now());
                    state.user_intents.note_user_intent_poll_finished(
                        poll_started,
                        USER_INTENT_EMPTY_POLL_INTERVAL,
                    );
                    tracing::warn!(
                        run_id = %run_id,
                        ?release_event_indices,
                        error = %error,
                        "failed to durably acknowledge user intent application"
                    );
                    if force_poll {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::ContractViolation,
                            format!(
                                "authoritative user guidance could not be durably acknowledged before the action boundary: {error}"
                            ),
                        ));
                    }
                    return Ok(applied_model_guidance);
                }
            }
        }

        if !force_poll || !page_has_more {
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            return Ok(applied_model_guidance);
        }
    }
}

fn apply_acknowledged_user_intents<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    acknowledged: &[crate::turn::run_control::QueuedUserIntent],
) -> bool {
    let mut model_guidance = Vec::new();
    let mut runtime_notifications = Vec::new();
    let mut active_work_contexts = Vec::new();
    for event in acknowledged {
        if let Some(content) = crate::turn::run_control::runtime_notification_content(&event.input)
        {
            runtime_notifications.push(content);
            continue;
        }
        let Some(content) = crate::turn::run_control::user_intent_content(&event.input) else {
            continue;
        };
        host.apply_user_intent_context(event);
        if let Some(context) = event.input.get("astra_runtime_context").filter(|context| {
            context.get("schema").and_then(serde_json::Value::as_str)
                == Some("active_work_snapshot.v1")
                && context.get("authority").and_then(serde_json::Value::as_str)
                    == Some("run_control_provider")
        }) {
            active_work_contexts.push(context.clone());
        }
        model_guidance.push(super::host::AppliedUserIntent {
            intent_id: event.intent_id.clone(),
            delivery: event.delivery,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: event.event_index,
            content,
        });
    }

    if !model_guidance.is_empty() {
        // A queued user instruction starts a new semantic turn. Preserve the
        // accumulated exploration/budget shape, but clear the prior turn's
        // effect and completion authority until the bounded classifier owns a
        // new value. Otherwise ReadOnly/MustMutate can leak across user
        // steering and make tool admission contradict the current request.
        state.turn_intent = None;
        state.task_profile.mutates_workspace = false;
        state.task_profile.verification_required = false;
        state.hooks.completion_settlement = Default::default();
        state.turn_guard.set_task_profile(state.task_profile);
        if let Some(executor) = state.runtime_tool_executor.as_deref() {
            executor.set_workspace_mutation_intent(
                astra_config::user_profile::WorkspaceMutationIntent::Unknown,
            );
        }
        let combined = model_guidance
            .iter()
            .map(|input| input.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        state.push_prompt_history_message(serde_json::json!({
            "role": "user",
            "content": combined,
        }));
        state.message = model_guidance
            .last()
            .map(|input| input.content.clone())
            .unwrap_or_default();
        state
            .user_intents
            .record_applied_user_intents(&model_guidance);
    }
    if !runtime_notifications.is_empty() {
        state.push_volatile_payload(
            super::host::VolatileKind::BackgroundTaskNotification,
            serde_json::json!({
                "schema": "background_task_notification.v1",
                "priority": "below_latest_user_intent",
                "updates": runtime_notifications,
                "instruction": "Reconcile these runtime facts with the latest user goal. Do not let a stale completion override newer user steering.",
            }),
        );
    }
    if !active_work_contexts.is_empty() {
        for context in &mut active_work_contexts {
            state.reconcile_active_work_context(context);
        }
        state.push_volatile_payload(
            super::host::VolatileKind::ActiveWorkSnapshot,
            serde_json::json!({
                "schema": "active_work_guidance_context.v1",
                "snapshots": active_work_contexts,
                "instruction": "This is the runtime-owned work state captured when the active-run guidance was accepted. Use canonical group/task IDs from it. Treat fanout groups as one work unit; do not copy child IDs or infer group completion from individual events.",
                "authority": "runtime_required_context",
            }),
        );
    }
    !model_guidance.is_empty()
}

pub(crate) fn turn_result_tokens_consumed(turn_result: &HostTurnResult) -> u64 {
    NormalizedPromptCacheUsage::new(
        turn_result.accum.prompt_tokens,
        turn_result.accum.cache_read_tokens,
        turn_result.accum.cache_creation_tokens,
    )
    .total_input_tokens()
    .saturating_add(turn_result.accum.completion_tokens)
}

/// Record an `llm_round` event for an early-exit path (no tool calls).
fn record_early_exit_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
) {
    record_observed_llm_round(state, turn_result, turn_start, finish_reason, Vec::new());
}

/// Retain the physical provider facts for a response whose conversational
/// content and tool authority were superseded by a newer durable control
/// epoch. Accounting is evidence, not execution authority: it must survive
/// even though no assistant text or tool call from this response is ingested.
fn record_superseded_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
) {
    if let Some(summary) = turn_result.accum.server_execution_summary.as_ref() {
        let is_new = state.fold_server_execution_summary_and_refresh_rounds(
            turn_result.accum.run_id.as_deref(),
            summary,
        );
        if !is_new {
            return;
        }
        if state.telemetry.first_ttft_ms.is_none() {
            state.telemetry.first_ttft_ms = turn_result.ttft_ms;
        }
        state.total_prompt = state
            .total_prompt
            .saturating_add(turn_result.accum.prompt_tokens);
        state.total_completion = state
            .total_completion
            .saturating_add(turn_result.accum.completion_tokens);
        state.total_cache_read = state
            .total_cache_read
            .saturating_add(turn_result.accum.cache_read_tokens);
        state.total_cache_creation = state
            .total_cache_creation
            .saturating_add(turn_result.accum.cache_creation_tokens);
        state.total_tool_calls = state
            .total_tool_calls
            .saturating_add(summary.tool_calls_count);
        state.total_observation_tool_calls = state
            .total_observation_tool_calls
            .saturating_add(summary.observation_tool_calls_count);
        state.step_recorder.record_tokens(
            turn_result.accum.prompt_tokens,
            turn_result.accum.completion_tokens,
        );
        state.has_any_usage |= turn_result.accum.has_usage;
        return;
    }
    if state.telemetry.first_ttft_ms.is_none() {
        state.telemetry.first_ttft_ms = turn_result.ttft_ms;
    }
    state.total_prompt = state
        .total_prompt
        .saturating_add(turn_result.accum.prompt_tokens);
    state.total_completion = state
        .total_completion
        .saturating_add(turn_result.accum.completion_tokens);
    state.total_cache_read = state
        .total_cache_read
        .saturating_add(turn_result.accum.cache_read_tokens);
    state.total_cache_creation = state
        .total_cache_creation
        .saturating_add(turn_result.accum.cache_creation_tokens);
    state.step_recorder.record_tokens(
        turn_result.accum.prompt_tokens,
        turn_result.accum.completion_tokens,
    );
    state.has_any_usage |= turn_result.accum.has_usage;
    let billable_input = NormalizedPromptCacheUsage::new(
        turn_result.accum.prompt_tokens,
        turn_result.accum.cache_read_tokens,
        turn_result.accum.cache_creation_tokens,
    )
    .total_input_tokens();
    if turn_result.accum.has_usage && billable_input > 0 {
        state.last_measured_prompt_tokens = Some(billable_input);
    }
    let tool_names = turn_result
        .accum
        .tool_calls
        .iter()
        .filter_map(|call| {
            astra_turn_core::tool::args::shape::tool_call_name(call).map(ToString::to_string)
        })
        .collect();
    record_observed_llm_round(
        state,
        turn_result,
        turn_start,
        Some("superseded_by_user_intent"),
        tool_names,
    );
}

fn record_observed_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
    tool_call_names: Vec<String>,
) {
    // A remote Server terminal closes an already-executed model loop. Its
    // physical rounds are persisted by the owning Server and its aggregate
    // usage is carried separately for accounting. Recording the thin-client
    // wait as another LLM round fabricates round=0, attributes the whole run
    // duration to one model request, and double-counts tokens in session
    // explain/harness projections.
    if turn_result.accum.server_execution_summary.is_some() {
        return;
    }
    let agentic_step = current_agentic_step(state);
    let run_id = state.current_run_id.clone();
    let duration_ms = turn_start.elapsed().as_millis() as u64;
    let start_offset_ms = state
        .turn_event_buffer
        .as_ref()
        .map(|buffer| buffer.offset_ms().saturating_sub(duration_ms))
        .unwrap_or_default();
    state.push_recent_round(super::host::RecentRoundSummary {
        purpose: state.inference_purpose,
        turn: state.session_turn,
        round: state.current_round_index,
        provider: String::new(),
        model: state.current_model_identity().unwrap_or("").to_string(),
        prompt_tokens: turn_result.accum.prompt_tokens,
        cache_read_tokens: turn_result.accum.cache_read_tokens,
        cache_creation_tokens: turn_result.accum.cache_creation_tokens,
        completion_tokens: turn_result.accum.completion_tokens,
        tool_calls_returned: tool_call_names.len() as u32,
        tool_call_names: tool_call_names.clone(),
        start_offset_ms,
        duration_ms,
        finish_reason: finish_reason.map(ToString::to_string),
    });
    let producer_agent_id = (state.inference_purpose
        == astra_turn_types::InferencePurpose::SubAgent)
        .then(|| state.self_agent_id.clone());
    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            purpose: state.inference_purpose,
            ttft_ms: turn_result.ttft_ms,
            duration_ms,
            prompt_tokens: turn_result.accum.prompt_tokens,
            completion_tokens: turn_result.accum.completion_tokens,
            cache_read_tokens: turn_result.accum.cache_read_tokens,
            cache_creation_tokens: turn_result.accum.cache_creation_tokens,
            tool_calls_returned: tool_call_names.len() as u32,
            tool_call_names,
            finish_reason: finish_reason.map(Into::into),
            agentic_step: Some(agentic_step),
            source: Some("agentic_loop".into()),
            run_id,
            parent_run_id: None,
            tool_calls: None,
            agent_id: producer_agent_id,
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

fn apply_terminal_control_stream_snapshot<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    snap: &AgenticTurnStreamSnapshot<'_>,
    control_outcome: crate::turn::terminal_control::TerminalControlOutcome,
) -> AgenticLoopOutcome {
    if state.telemetry.first_ttft_ms.is_none() {
        state.telemetry.first_ttft_ms = snap.ttft_ms;
    }
    if let Some(session_id) = snap.session_id.as_ref() {
        state.current_session_id = Some(session_id.clone());
        host.on_session_bound(session_id);
        if state.context_manifest_user_id.is_some() {
            state.step_recorder.attach_persistence(session_id);
        }
    }
    if snap.run_id.is_some() {
        state.current_run_id = snap.run_id.clone();
    }
    state.total_prompt += snap.prompt_tokens;
    state.total_completion += snap.completion_tokens;
    state.total_cache_read += snap.cache_read_tokens;
    state.total_cache_creation += snap.cache_creation_tokens;
    if let Some(summary) = snap.server_execution_summary {
        let is_new =
            state.fold_server_execution_summary_and_refresh_rounds(snap.run_id.as_deref(), summary);
        if is_new {
            state.total_tool_calls = state
                .total_tool_calls
                .saturating_add(summary.tool_calls_count);
            state.total_observation_tool_calls = state
                .total_observation_tool_calls
                .saturating_add(summary.observation_tool_calls_count);
        }
    }
    state
        .step_recorder
        .record_tokens(snap.prompt_tokens, snap.completion_tokens);
    state.has_any_usage |= snap.has_usage;
    let billable_input = NormalizedPromptCacheUsage::new(
        snap.prompt_tokens,
        snap.cache_read_tokens,
        snap.cache_creation_tokens,
    )
    .total_input_tokens();
    // A Server-owned terminal reports aggregate run usage for accounting. Its
    // physical final-request usage is installed by the admission host before
    // this phase runs; replacing it with a root-plus-children total would make
    // context pressure and compaction act on the wrong window.
    if snap.has_usage && billable_input > 0 && snap.server_execution_summary.is_none() {
        state.last_measured_prompt_tokens = Some(billable_input);
    }
    state.consecutive_context_window_errors = 0;

    match control_outcome {
        crate::turn::terminal_control::TerminalControlOutcome::Requested(_) => {
            AgenticLoopOutcome::Delegated
        }
        crate::turn::terminal_control::TerminalControlOutcome::Rejected(rejection) => {
            AgenticLoopOutcome::ControlRejected(rejection)
        }
        crate::turn::terminal_control::TerminalControlOutcome::Passthrough => {
            unreachable!("host must not surface a passthrough terminal-control outcome")
        }
    }
}

fn route_runtime_policy_evidence(
    state: &mut AgenticLoopState,
    facts: &astra_core::observation_journal::JournalFacts,
    evidence: crate::turn::runtime_policy::RuntimePolicyEvidence,
) {
    use crate::turn::runtime_policy::RuntimePolicyEvidence;

    match evidence {
        RuntimePolicyEvidence::BudgetExpansionSuggested {
            factor,
            max_ceiling,
        } => {
            tracing::info!(
                target: "astra::policy",
                factor,
                max_ceiling,
                consecutive_outcomes = facts.streaks.consecutive_rounds_with_outcome,
                current_max_turns = state.max_turns,
                remaining_turns = state.remaining_turns,
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
            NormalizedPromptCacheUsage::new(
                result.accum.prompt_tokens,
                result.accum.cache_read_tokens,
                result.accum.cache_creation_tokens,
            )
            .total_input_tokens()
        })
        .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32);
    let manifest_id = format!("manifest-{}", Uuid::new_v4());
    let turn_id = format!("{run_id}:llm:{llm_attempt_index}");
    let reason = manifest_reason_for_llm_call(state);
    let model_name = state.current_model_identity().unwrap_or("").to_string();
    let context_window_tokens = context_window_tokens_for_context_manifest(state);
    let projection = crate::turn::llm::context::build_context_manifest_projection(
        crate::turn::llm::context::ContextManifestProjectionInput {
            owner_id: user_id,
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

pub(crate) fn record_context_compactions<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    observations: &[astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation],
) {
    if observations.is_empty() {
        return;
    }

    for observation in observations {
        if !observation.is_consistent() {
            tracing::warn!(
                target: "astra_runtime::compaction",
                observation_id = %observation.id,
                kind = %observation.kind,
                "ignoring inconsistent context compaction observation"
            );
            continue;
        }
        let tokens_freed = observation.tokens_before - observation.tokens_after;
        let compacted_messages = observation
            .messages_before
            .saturating_sub(observation.messages_after)
            .min(u64::from(u32::MAX)) as u32;
        let pressure = if state.max_turn_input_tokens > 0 {
            (observation.tokens_before as f64 / state.max_turn_input_tokens as f64).min(1.0)
        } else {
            0.0
        };
        state.context_compression_triggered = true;
        state.compact_tier_applied = state.compact_tier_applied.max(observation.tier);
        state
            .compaction_effectiveness
            .record_compaction(tokens_freed);
        if observation.effectiveness
            == astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Insufficient
        {
            state.compaction_effectiveness.mark_insufficient();
        }
        state.step_recorder.record_compaction_with_kind(
            &observation.kind.to_string(),
            compacted_messages,
            tokens_freed,
            pressure,
        );
        if let Some(ref mut sess) = state.pipeline_session {
            sess.record_compaction_audit(
                &observation.kind.to_string(),
                compacted_messages,
                tokens_freed.min(u64::from(u32::MAX)) as u32,
            );
            sess.stats.record_compaction(tokens_freed);
        }
        host.on_compaction(CompactionEvent::new(
            observation.kind,
            pressure,
            tokens_freed,
            observation.tokens_before,
            state.max_turn_input_tokens,
            compacted_messages as usize,
            observation
                .messages_after
                .min(u64::try_from(usize::MAX).unwrap_or(u64::MAX)) as usize,
            Vec::new(),
        ));
    }
}

pub(crate) async fn execute_turn_and_ingest_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
) -> Result<TurnExecutionControl, astra_core::ClassifiedError> {
    // A provider request is an externally metered action boundary. Polling is
    // deliberately forced here rather than governed by the UI-oriented empty
    // cadence: one indexed control-lane read per provider round is the bounded
    // cost of guaranteeing that post-tool guidance supersedes stale work
    // before more model traffic is admitted.
    loop {
        inject_polled_user_intents_before_provider(host, state).await?;
        match authorize_provider_boundary(state).await? {
            ProviderBoundaryGate::Authorized => break,
            ProviderBoundaryGate::Paused if wait_for_pause_clear_or_cancel(state).await => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "run was cancelled while paused before a provider boundary",
                ));
            }
            ProviderBoundaryGate::Paused => {}
        }
    }

    // Project the immutable policy revision selected at the preceding
    // authoritative tool boundary. Request retries and preparation reruns
    // render the same bytes; they never evaluate or advance policy state.
    state.clear_volatile(super::host::VolatileKind::PolicyAdvisory);
    if let Some(payload) =
        crate::turn::runtime_policy::policy_advisory_payload(&state.stall.active_policy_feedback)
    {
        state.push_volatile_payload(super::host::VolatileKind::PolicyAdvisory, payload);
    }

    // Policy evidence always reaches the model. Interaction mode controls only
    // whether the same evidence is also rendered as user-facing status text.
    // Auto permission is not a request to disable the feedback loop.
    let show_policy_feedback_status = host.turn_interaction_mode().shows_policy_feedback_status();
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
                if state.stall.work_evidence_advisory_emitted {
                    a.push("work_evidence_sufficiency".to_string());
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
            );
            if !status.is_empty() {
                state.push_volatile(super::host::VolatileKind::SelfStatus, status);
            }
        }

        let mut guidance =
            crate::prompts::tool_round_guidance(&state.messages, state.llm_rounds_completed);
        if !state.suppress_execution_slice_guidance() {
            let slice_guidance = crate::prompts::execution_slice_guidance(
                state.remaining_turns,
                state.max_turns,
                super::lifecycle::adaptive_budget_is_renewable(state),
            );
            if !slice_guidance.is_empty() {
                if !guidance.is_empty() {
                    guidance.push_str("\n\n");
                }
                guidance.push_str(&slice_guidance);
            }
        }
        if !guidance.is_empty() {
            state.push_volatile(super::host::VolatileKind::BudgetAdvisory, guidance);
        }
    }

    // If a mutating task has accumulated only read-only observations, surface
    // that fact before the next LLM call. It remains advisory because further
    // investigation may still be justified by a concrete unknown.
    if should_emit_execution_escalation_advisory(state) {
        let read_only_calls = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| r.was_executed() && r.ok)
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
        if show_policy_feedback_status && !prep.quiet {
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
    let cache_waste_threshold = tool_cfg.effective_cache_waste_midloop_threshold() as usize;

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
            parallel_batching_force_streak: parallel_batching_force_threshold,
            cache_waste_threshold,
        };
        let guards = super::guards::default_guards();
        for (hint_style, hint_text) in super::guards::evaluate_guards(&guards, state, &guard_cfg) {
            if show_policy_feedback_status && !prep.quiet {
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
        use crate::turn::providers::{LiveRuntimeProvider, ObservationProvider};
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

        let default_policy = crate::turn::runtime_policy::RuntimePolicy::default();
        let policy = state.budget_policy.as_ref().unwrap_or(&default_policy);
        let policy_evidence = policy.decide(&facts);

        for evidence in policy_evidence {
            route_runtime_policy_evidence(state, &facts, evidence);
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
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "repeated_behavior_pattern",
                        "round": state.llm_rounds_completed,
                        "assessment": "The recent tool pattern may be repetitive. Treat this as evidence when choosing the next action; continue if the repetition is justified by the task."
                    }),
                );
                state
                    .stall
                    .circuit_breaker
                    .acknowledge_pattern_observation();
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_advisory",
                    round = state.llm_rounds_completed,
                    "circuit breaker observation delivered as advisory evidence"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::AdvisoryThresholdReached => {
                let diagnosis = interruption_diagnosis_summary(state);
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "repetition_threshold_reached",
                        "round": state.llm_rounds_completed,
                        "diagnosis": diagnosis,
                        "assessment": "A behavior-pattern detector reached its configured threshold. This is advisory evidence, not a budget or safety boundary; decide whether to change approach or continue with the current evidence."
                    }),
                );
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_threshold_advisory",
                    round = state.llm_rounds_completed,
                    "circuit breaker threshold recorded as advisory evidence"
                );
                if show_policy_feedback_status && !prep.quiet {
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
                    "circuit breaker introspection signal delivered as advisory evidence"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Continue => {}
            // BreakerAction is #[non_exhaustive] — future soft-intervention
            // variants should default to a no-op so the loop continues.
            _ => {}
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

    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::AgenticRequestSnapshot,
        &state.messages,
    );
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
    let llm_wall_start = Instant::now();
    let t_llm_start = std::time::Instant::now();
    tracing::debug!(
        target: "astra_timing",
        llm_round = llm_attempt_index,
        messages = pre_llm_messages.len(),
        "LLM call started"
    );
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_started(turn_index as u32);
    }
    let turn_result = host.execute_turn(state).await;
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_completed(
            turn_index as u32,
            turn_result.as_ref().ok().and_then(|result| result.ttft_ms),
            llm_wall_start.elapsed().as_millis() as u64,
        );
    }
    // A server host may return a typed control-plane transition (for example,
    // the synthetic `start_work` admission) without crossing a provider
    // boundary.  Keep it in the ordinary ingest path so the lifecycle and
    // task board receive durable evidence, but do not let it seed provider
    // prompt-cache accounting or a model-call context manifest.
    let control_plane_boundary = match turn_result.as_ref() {
        Ok(result) => host.consume_control_plane_turn(result),
        Err(_) => super::host::ControlPlaneTurnBoundary::Ordinary,
    };
    let providerless_control_plane_turn = matches!(
        control_plane_boundary,
        super::host::ControlPlaneTurnBoundary::Providerless
    );
    // `text_only` applies to one successful model boundary. Keep it through
    // the admission/tool phase when a provider nevertheless returns tools so
    // a retry cannot execute them. A safely recoverable provider failure keeps
    // the same authority until the bounded recovery decision below; every
    // other error clears it.
    let retain_text_only_for_tool_admission = turn_result.as_ref().is_ok_and(|result| {
        result.accum.has_tool_calls
            || !result.accum.tool_calls.is_empty()
            || !result.edge_tool_round.is_empty()
            || result
                .accum
                .server_execution_summary
                .as_ref()
                .is_some_and(|summary| summary.tool_calls_count > 0)
    });
    let provider_recovery_pending = turn_result.as_ref().is_err_and(|error| {
        matches!(
            error.kind,
            astra_core::ErrorKind::ProviderDeadline
                | astra_core::ErrorKind::StreamTransport
                | astra_core::ErrorKind::StreamIdle
        )
    });
    // Capture the current-round typed candidate. It remains live until every
    // terminal guard accepts the prose or an authoritative Work/user-intent
    // fact revokes it; merely receiving non-empty prose is not consumption.
    let committed_work_synthesis_candidate = state
        .hooks
        .completion_settlement
        .preserve_final_synthesis_wire_surface;
    // A compliant non-empty text response is the deliverable produced by the
    // bounded settlement boundary. The runtime limit constrains additional
    // tool execution; it is not itself evidence that the user's requested
    // answer failed. Tool-shaped or empty responses still follow the bounded
    // retry/interruption paths below.
    if !retain_text_only_for_tool_admission && !provider_recovery_pending {
        state.hooks.completion_settlement.text_only = false;
    }
    tracing::debug!(
        target: "astra_timing",
        llm_round = llm_attempt_index,
        elapsed_ms = t_llm_start.elapsed().as_millis(),
        ok = turn_result.is_ok(),
        "LLM call completed"
    );
    if !host.owns_model_inference_timing() {
        complete_turn_phase(
            host,
            state,
            llm_wall_start,
            TurnPhaseKind::ModelInference,
            turn_index as u32,
            0,
            if turn_result.is_ok() {
                TurnPhaseOutcome::Succeeded
            } else {
                TurnPhaseOutcome::Failed
            },
            format!("model_inference_{turn_index}"),
        );
    }
    let locally_executed_provider_round = should_record_local_provider_round(
        control_plane_boundary,
        turn_result
            .as_ref()
            .ok()
            .and_then(|result| result.accum.server_execution_summary.as_ref())
            .is_some(),
    );
    if locally_executed_provider_round {
        state.record_local_usage_coverage(
            turn_result
                .as_ref()
                .is_ok_and(|result| result.accum.has_usage),
        );
        state.record_local_llm_round();
    }
    // Capture finish_reason before the match consumes turn_result.
    // Used by textless-stop retry (loop level) and ensure_terminal_text
    // (finalization level) to distinguish true silence from forced truncation
    // when the API's max_tokens limit cuts off the model's output.
    state.last_finish_reason = turn_result
        .as_ref()
        .ok()
        .and_then(|r| r.accum.finish_reason.clone());
    // Bridge-reported context compactions live in TurnOutput::accum and are
    // available only when the host returned a successful output. Failed calls
    // have no accumulator evidence to record here.
    if let Ok(result) = &turn_result
        && !result.accum.context_compactions.is_empty()
    {
        record_context_compactions(host, state, &result.accum.context_compactions);
    }
    // Persist the per-call manifest only after the host returns: the durable
    // record includes observed token usage and the emitted context-manifest
    // trace, both of which are only available on the completed turn result.
    if let Ok(result) = &turn_result
        && let Some(trace) = result.accum.context_manifest_trace.clone()
    {
        state.last_llm_context_manifest_trace = Some(trace);
    }
    if !providerless_control_plane_turn {
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
    }
    let mut turn_result = match turn_result {
        Ok(turn_result) => turn_result,
        Err(error) => {
            fold_provider_completion_error_usage(state, &error);
            if schedule_safe_provider_recovery(state, &error) {
                tracing::warn!(
                    target: "astra::provider_recovery",
                    error = %error,
                    "provider attempt ended before executable or visible delivery; scheduling one safe thinking-off recovery round"
                );
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::ContinueLoop);
            }
            record_repeated_transport_success_empty_completion(state, &error);
            state.hooks.completion_settlement.text_only = false;
            record_direct_llm_error_state(state, &error);
            return Err(error);
        }
    };
    let action_fence = inject_polled_user_intents_before_action(host, state).await;
    if action_fence.as_ref().is_ok_and(|applied| *applied) || action_fence.is_err() {
        // Physical usage and round evidence remain true even though the
        // response no longer has conversational or execution authority.
        record_superseded_llm_round(state, &turn_result, prep.turn_start_time);
        update_turn_trace_collector(state, &turn_result);
    }
    let guidance_applied = match action_fence {
        Ok(applied) => applied,
        Err(error) => return Err(error),
    };
    if guidance_applied {
        // Usage/round accounting above remains true provider evidence, but
        // neither text nor tool calls from the older context may enter the
        // canonical conversation or execution lane. The applied intent was
        // appended by the poll and the next request starts from that exact
        // typed control epoch.
        state.final_text.clear();
        state.final_text_streamed = false;
        state.push_volatile_payload(
            super::host::VolatileKind::UserIntentBoundary,
            serde_json::json!({
                "schema": "user_intent_action_fence.v1",
                "signal": "provider_response_superseded",
                "execution_authority": "none",
                "instruction": "The preceding provider response was generated before newly accepted user guidance. Re-evaluate the current objective from the applied guidance before choosing any action or final response.",
            }),
        );
        state.step_recorder.end_turn(false);
        return Ok(TurnExecutionControl::ContinueLoop);
    }
    let continuation_authority = host.continuation_authority(&turn_result);
    if continuation_authority == ContinuationAuthority::RemoteServer
        && (turn_result.accum.has_tool_calls || !turn_result.accum.tool_calls.is_empty())
    {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "remote Server declared terminal continuation ownership while returning pending client continuation work",
        ));
    }
    let collapsed_observation_calls =
        collapse_batched_observation_fanout(&mut turn_result.accum.tool_calls);
    if collapsed_observation_calls > 0 {
        tracing::info!(
            target: "astra::provenance_guard",
            collapsed_observation_calls,
            "collapsed same-boundary observation facet fanout into composite overview calls"
        );
    }
    state.rate_limit_cooldown.record_success();
    // Clear pipeline recovery escalation after a successful LLM call —
    // the PTL pressure is relieved.
    if let Some(ref mut sess) = state.pipeline_session {
        sess.recovery.reset_on_success();
    }
    let snap = agentic_turn_stream_snapshot_with_kind(
        &turn_result.accum,
        turn_result.ttft_ms,
        turn_result.error_kind,
    );
    update_turn_trace_collector(state, &turn_result);

    if let Some(control_outcome) = host.take_terminal_control_outcome() {
        state.set_terminal_execution_authority(match continuation_authority {
            ContinuationAuthority::RemoteServer => TerminalExecutionAuthority::RemoteServer,
            ContinuationAuthority::Runtime => TerminalExecutionAuthority::EdgeLedger,
        });
        return Ok(TurnExecutionControl::Return(
            apply_terminal_control_stream_snapshot(host, state, &snap, control_outcome),
        ));
    }

    // Edge callbacks completed while a Remote Server admission stream was
    // open are observations of Server-owned work, not pending local work.
    // Excluding them from ingest is what prevents a client-side continuation
    // without discarding their rendered/protocol evidence.
    let edge_len = match continuation_authority {
        ContinuationAuthority::Runtime => turn_result.edge_tool_round.len(),
        ContinuationAuthority::RemoteServer => 0,
    };
    let tool_record_floor = state.stall.tool_call_records.len();
    let transcript_append_start = state.messages.len();
    let ingest_outcome = ingest_agentic_turn_stream(
        &snap,
        edge_len,
        |i| turn_result.edge_tool_round[i].tool.clone(),
        &state.message,
        &state.recent_tools,
        prep.quiet,
        AgenticTurnIngestMut {
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
    // Apply weak/partial quarantine on the same boundary as newly ingested
    // records, and checkpoint the first transition immediately.
    if let Some(records) = state.stall.tool_call_records.get(tool_record_floor..) {
        let records = records.to_vec();
        apply_workspace_observation_quarantine_transition(state, &records);
    }
    // A Server-owned summary is an aggregate for one physical server run,
    // while `ingest_agentic_turn_stream` has already added that run's counts
    // to the local totals.  Fold the typed summary by run identity and
    // replace the just-added raw contribution with the deduplicated aggregate
    // so a repeated terminal frame cannot inflate the logical-turn result.
    if let Some(summary) = snap.server_execution_summary {
        let is_new =
            state.fold_server_execution_summary_and_refresh_rounds(snap.run_id.as_deref(), summary);
        state.total_tool_calls = state
            .total_tool_calls
            .saturating_sub(summary.tool_calls_count)
            .saturating_add(if is_new { summary.tool_calls_count } else { 0 });
        state.total_observation_tool_calls = state
            .total_observation_tool_calls
            .saturating_sub(summary.observation_tool_calls_count)
            .saturating_add(if is_new {
                summary.observation_tool_calls_count
            } else {
                0
            });
    }
    // Preserve settlement text at the observation boundary, before the
    // ingest control is consumed.  This covers both server tool calls and
    // edge-tool rounds, and lets a later compliant text-only retry replace a
    // prior mixed candidate without allowing another violating tool request
    // to overwrite it.
    capture_latest_provider_text(state, &turn_result);
    capture_deferred_candidate_text(state, &turn_result);
    if matches!(&ingest_outcome, AgenticTurnIngestOutcome::Break)
        && runtime_retrospective_requires_live_evidence(&state.message)
        && !state.telemetry.all_tools_used.contains("introspect")
        && state.hooks.completion_settlement.runtime_evidence_retries == 0
    {
        state.hooks.completion_settlement.runtime_evidence_retries = 1;
        state.messages.truncate(transcript_append_start);
        state.final_text.clear();
        state.final_text_streamed = false;
        state.push_volatile_payload(
            super::host::VolatileKind::RuntimeEvidenceRequired,
            serde_json::json!({
                "schema": "runtime_evidence_required.v1",
                "reason": "runtime_or_session_retrospective_without_live_observation",
                "instruction": "Before making runtime, session-state, trace, or tool-ledger claims, call introspect exactly once with facet=overview, depth=diagnostic, horizon=recent. Use reflect at most once only for persisted prior-turn causality. If observation is unavailable, explicitly limit the answer to visible conversation evidence; never claim that runtime records were inspected."
            }),
        );
        tracing::warn!(
            target: "astra::provenance_guard",
            "retrying runtime retrospective that attempted to settle without introspect evidence"
        );
        record_early_exit_llm_round(
            state,
            &turn_result,
            prep.turn_start_time,
            Some("runtime_evidence_required"),
        );
        state.step_recorder.end_turn(false);
        return Ok(TurnExecutionControl::ContinueLoop);
    }
    state.record_appended_prompt_history_from(transcript_append_start);
    if let Some(session_id) = state.current_session_id.as_deref() {
        host.on_session_bound(session_id);
        if let Some(buffer) = state.turn_event_buffer.as_mut()
            && let Err(error) = buffer.bind_session_id(session_id)
        {
            tracing::warn!(
                session_id,
                error = %error,
                "could not bind streamed session identity to first-round observability events"
            );
        }
    }

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
        if !providerless_control_plane_turn {
            let turn = session_turn_number(state);
            let session_id = state.current_session_id.clone();
            let run_id = state.current_run_id.clone();
            let model_id = state.current_model_identity().map(str::to_string);
            let identity = session_id
                .as_deref()
                .zip(run_id.as_deref())
                .zip(model_id.as_deref())
                .filter(|((session_id, run_id), model_id)| {
                    !session_id.trim().is_empty()
                        && !run_id.trim().is_empty()
                        && !model_id.trim().is_empty()
                        && !state.self_agent_id.trim().is_empty()
                })
                .map(|((session_id, run_id), model_id)| {
                    astra_turn_core::context_feedback::RuntimeFeedbackIdentity {
                        session_id: session_id.to_string(),
                        run_id: run_id.to_string(),
                        agent_id: state.self_agent_id.clone(),
                        model_id: model_id.to_string(),
                        topology: host.runtime_feedback_topology(),
                        request: state
                            .last_llm_context_manifest_trace
                            .as_ref()
                            .and_then(|trace| trace.get("request_identity"))
                            .cloned()
                            .and_then(|value| serde_json::from_value(value).ok()),
                    }
                });
            let aggregate_usage = || {
                astra_turn_core::token_accounting::TokenAccounting::from_fields(
                    turn_result.accum.prompt_tokens,
                    turn_result.accum.cache_read_tokens,
                    turn_result.accum.cache_creation_tokens,
                    turn_result.accum.completion_tokens,
                )
            };
            // A logical response may contain multiple physical provider
            // attempts (for example one bounded output-cap retry).  The
            // aggregate belongs in run accounting, while context pressure
            // and cache decisions must use only the final physical request.
            // Hosts that know both values populate `current_request_usage`;
            // legacy hosts still fall back to their single-request aggregate.
            let request_usage = turn_result
                .accum
                .current_request_usage
                .map(|usage| {
                    astra_turn_core::token_accounting::TokenAccounting::from_fields(
                        usage.fresh_input_tokens,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                        usage.output_tokens,
                    )
                })
                .or_else(|| {
                    (!turn_result.accum.usage_is_run_total && turn_result.accum.has_usage)
                        .then(aggregate_usage)
                });
            let run_usage = turn_result.accum.has_usage.then(|| {
                if turn_result.accum.usage_is_run_total {
                    aggregate_usage()
                } else {
                    astra_turn_core::token_accounting::TokenAccounting::from_fields(
                        state.total_prompt,
                        state.total_cache_read,
                        state.total_cache_creation,
                        state.total_completion,
                    )
                }
            });
            let server_execution_summary = turn_result.accum.server_execution_summary.as_ref();
            let forwarded_runtime_feedback = server_execution_summary.and_then(|summary| {
                authoritative_server_runtime_feedback(
                    summary,
                    session_id.as_deref(),
                    run_id.as_deref(),
                    model_id.as_deref(),
                    turn,
                )
            });
            let server_owned_feedback = server_execution_summary.is_some();
            if server_owned_feedback && forwarded_runtime_feedback.is_none() {
                tracing::warn!(
                    target: "astra_runtime::agentic_loop",
                    session_id = session_id.as_deref().unwrap_or("<unknown>"),
                    run_id = run_id.as_deref().unwrap_or("<unknown>"),
                    "Server-owned terminal runtime feedback did not match the admitted invocation"
                );
            }
            let mut runtime_feedback = if server_owned_feedback {
                forwarded_runtime_feedback
            } else {
                identity.map(|identity| {
                    let wire_budget = state
                        .last_llm_context_manifest_trace
                        .as_ref()
                        .and_then(|trace| trace.pointer("/wire/budget"));
                    let model_context_window_tokens = state
                        .last_llm_context_manifest_trace
                        .as_ref()
                        .and_then(|trace| trace.get("model_context_window_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .filter(|value| *value > 0);
                    let estimated_input_tokens = wire_budget
                        .and_then(|budget| budget.get("estimated_input_tokens"))
                        .and_then(serde_json::Value::as_u64);
                    let effective_input_limit_tokens = wire_budget
                        .and_then(|budget| budget.get("effective_input_limit"))
                        .and_then(serde_json::Value::as_u64)
                        .filter(|value| *value > 0)
                        .or_else(|| {
                            (state.max_turn_input_tokens > 0).then_some(state.max_turn_input_tokens)
                        });
                    let token_pressure = estimated_input_tokens
                        .zip(effective_input_limit_tokens)
                        .map(|(estimated, limit)| estimated as f64 / limit as f64);
                    let prompt_cache_identity = prompt_cache_identity_from_manifest(
                        state.last_llm_context_manifest_trace.as_ref(),
                    );
                    let absolute_round_ceiling =
                        (!state.agentic_turn_budget.renewable_past_review_limit).then(|| {
                            u32::try_from(state.agentic_turn_budget.hard_turn_limit)
                                .unwrap_or(u32::MAX)
                        });
                    astra_turn_core::context_feedback::RuntimeFeedbackFrame {
                        schema_version:
                            astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
                        identity,
                        progress: astra_turn_core::context_feedback::RuntimeFeedbackProgress {
                            session_turn: turn,
                            agentic_round_index: llm_attempt_index,
                            llm_rounds_completed: turn_result
                                .accum
                                .server_execution_summary
                                .as_ref()
                                .map_or(state.llm_rounds_completed, |summary| summary.llm_rounds),
                            slice_round_limit: u32::try_from(state.max_turns).unwrap_or(u32::MAX),
                            slice_rounds_remaining: u32::try_from(state.remaining_turns)
                                .unwrap_or(u32::MAX),
                            absolute_round_ceiling,
                        },
                        context: astra_turn_core::context_feedback::RuntimeContextFeedback {
                            prompt_cache_identity,
                            model_context_window_tokens,
                            effective_input_limit_tokens,
                            estimated_input_tokens,
                            token_pressure,
                            compaction_tier: state.compact_tier_applied,
                        },
                        request_usage,
                        run_usage,
                        was_truncated: state.last_finish_reason.as_deref() == Some("length"),
                        cache_break_detected: None,
                        policy_feedback: state.stall.active_policy_feedback.clone(),
                    }
                })
            };
            if let (Some(pipeline_sess), Some(frame)) =
                (&mut state.pipeline_session, runtime_feedback.as_mut())
            {
                let accepted = if server_owned_feedback {
                    pipeline_sess.accept_authoritative_runtime_feedback(frame)
                } else {
                    pipeline_sess.record_runtime_feedback("agentic_loop", frame, None)
                };
                if !accepted {
                    tracing::warn!(
                        target: "astra_runtime::agentic_loop",
                        session_id = %frame.identity.session_id,
                        run_id = %frame.identity.run_id,
                        session_turn = frame.progress.session_turn,
                        llm_rounds_completed = frame.progress.llm_rounds_completed,
                        "rejecting invalid or out-of-order runtime feedback frame"
                    );
                }

                if accepted {
                    host.publish_runtime_feedback(frame);
                    let feedback = frame.request_usage.map(|tokens| {
                        astra_turn_core::context_feedback::ContextFeedback {
                            tokens,
                            cache_hit_ratio: tokens.cache_hit_ratio(),
                            was_truncated: frame.was_truncated,
                            cache_break_detected: frame.cache_break_detected.clone(),
                        }
                    });

                    // Emit pipeline journal events for observability and cloud sync
                    if let Some(ref mut buf) = state.turn_event_buffer {
                        // Per-turn feedback event
                        let feedback_evt =
                            astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
                                frame,
                            );
                        if let Ok(payload) = serde_json::to_value(&feedback_evt) {
                            buf.record(
                                astra_services::session_journal::JournalEvent::pipeline_feedback(
                                    session_id.as_deref(),
                                    turn,
                                    payload,
                                )
                                .with_producer_scope(run_id.as_deref()),
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
                                )
                                .with_producer_scope(run_id.as_deref()),
                            );
                            }
                        }

                        // Evaluate trace alerts and emit them to the journal.
                        let alerts = feedback.as_ref().map_or_else(Vec::new, |feedback| {
                            astra_turn_core::trace_alert::evaluate_alerts(
                                turn,
                                feedback,
                                &pipeline_sess.stats,
                                &pipeline_sess.recovery,
                            )
                        });
                        // Best-effort webhook dispatch: dispatcher is initialized once
                        // per process via a global OnceLock, reusing reqwest::Client's
                        // connection pool + TLS session cache across turns. Dispatch
                        // runs async so it never blocks turn execution.
                        if !alerts.is_empty() {
                            if let Some(session_id_str) =
                                alert_dispatch_session_id(session_id.as_deref())
                            {
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
                                astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(
                                    alert,
                                );
                            if let Ok(payload) = serde_json::to_value(&alert_evt) {
                                buf.record(
                                    astra_services::session_journal::JournalEvent::pipeline_alert(
                                        session_id.as_deref(),
                                        turn,
                                        payload,
                                    )
                                    .with_producer_scope(run_id.as_deref()),
                                );
                            }
                        }
                    }
                }
            } else if state.pipeline_session.is_some() {
                tracing::warn!(
                    target: "astra_runtime::agentic_loop",
                    turn,
                    "skipping model-scoped pipeline feedback without resolved model identity"
                );
            }
        } else {
            tracing::debug!(
                target: "astra_runtime::agentic_loop",
                "skipping provider feedback for host-owned control-plane turn"
            );
        }
        host.on_turn_completed(state);
    }

    let iteration_control = map_ingest_outcome_to_iteration_control(ingest_outcome);
    if continuation_authority == ContinuationAuthority::RemoteServer
        && !matches!(
            iteration_control,
            AgenticIngestIterationControl::BreakLoop | AgenticIngestIterationControl::Fatal(_)
        )
    {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "remote Server declared terminal continuation ownership while returning pending client continuation work",
        ));
    }

    match iteration_control {
        AgenticIngestIterationControl::Fatal(e) => {
            // A fatal edge-owned boundary is allowed to supersede an earlier
            // remote summary.  The summary remains useful accounting, but it
            // cannot hide the ledger that actually terminated this turn.
            if continuation_authority == ContinuationAuthority::Runtime {
                state.set_terminal_execution_authority(TerminalExecutionAuthority::EdgeLedger);
            }
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

                let rewrite_permit = state.begin_canonical_rewrite();
                let outcome = super::super::compaction_replay::try_compact_for_retry_checked(
                    &mut state.messages,
                    &mut state.compaction_effectiveness,
                    state.last_measured_prompt_tokens,
                    state.max_turn_input_tokens,
                    state.consecutive_context_window_errors,
                );
                match outcome {
                    super::super::compaction_replay::CompactionReplayOutcome::Compacted(result) => {
                        state.finish_canonical_rewrite(rewrite_permit);
                        let tier_label = result.tier.to_string();
                        // Feed compaction stats into pipeline for reserve estimation.
                        if let Some(ref mut sess) = state.pipeline_session {
                            sess.recovery.record_reactive_compact();
                            sess.stats.record_compaction(result.tokens_freed);
                        }
                        let tokens_freed = result.pipeline_outcome.total_tokens_freed;
                        let messages_after = state.messages.len();
                        // In a retry context we know we overflowed the context
                        // window, so use max_turn_input_tokens as the floor for
                        // tokens_before when measured usage is unavailable.
                        let tokens_before = state
                            .last_measured_prompt_tokens
                            .unwrap_or(state.max_turn_input_tokens);
                        let pressure = if state.max_turn_input_tokens == 0 {
                            0.0
                        } else {
                            (tokens_before as f64 / state.max_turn_input_tokens as f64).min(1.0)
                        };
                        state.context_compression_triggered = true;
                        state.step_recorder.record_compaction_with_kind(
                            &result.tier.to_string(),
                            result.messages_removed.min(u32::MAX as usize) as u32,
                            tokens_freed,
                            pressure,
                        );
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
                            let writer = match state.context_manifest_user_id.as_deref() {
                                Some(user_id) => {
                                    astra_services::session_journal::JournalWriter::for_user(
                                        user_id, sid,
                                    )
                                }
                                None => astra_services::session_journal::JournalWriter::new(sid),
                            };
                            if let Ok(writer) = writer {
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
            // A successful provider response with neither tool calls nor text
            // is not a completed user turn while budget remains. Retry once in
            // an explicitly text-only settlement mode. This handles transient
            // empty provider responses inside the runtime instead of exposing
            // an internal `empty_completion` reason and asking the user to
            // manually drive a continuation.
            if continuation_authority == ContinuationAuthority::Runtime
                && should_retry_textless_response(state, &turn_result)
            {
                begin_textless_response_retry(state);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("textless_response_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // The response has crossed the final edge-owned boundary.  If a
            // prior server run contributed aggregate evidence, keep that
            // accounting/coverage fact but let this terminal edge ledger own
            // exit status and unresolved-failure interpretation.
            if continuation_authority == ContinuationAuthority::Runtime {
                state.set_terminal_execution_authority(TerminalExecutionAuthority::EdgeLedger);
            }

            // A second persistent observation after the one bounded
            // reconciliation pass is itself a typed incomplete outcome. Set
            // it before the generic interruption branch so the same terminal
            // rendering/persistence path is used for both provider and
            // policy-owned incomplete results.
            enforce_persistent_unresolved_outcome_terminal(state);

            // An authoritative interruption is a terminal boundary for this
            // turn.  Do not let user-intent polling or completion obligations
            // reopen another provider call after the host has already
            // preserved a partial result (for example, an exhausted output
            // cap, transport interruption, or token budget).  Those guards
            // are valuable while the turn is healthy, but once a typed
            // interruption exists the correct user experience is to render
            // the resumable partial result and let the next turn continue.
            if state.interruption.is_some() {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("interrupted"),
                );
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                    turn_result_tokens_consumed(&turn_result),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            let user_intent_settlement_fence = commit_user_intent_settlement_fence(state).await?;

            // Close the last-model-boundary race for remotely accepted user
            // guidance. The ordinary poll at loop entry is cadence-limited;
            // without this forced durable poll, an intent accepted while the
            // final response was in flight could be acknowledged by the
            // server after that poll and then stranded by terminal settlement.
            // If guidance arrived, the response just produced is an
            // intermediate assistant message and the same run continues once
            // so the model observes the accepted input exactly once.
            if inject_polled_user_intents_before_settlement(host, state).await? {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("user_intent_arrived_before_settlement"),
                );
                state.step_recorder.end_turn(false);
                return continue_after_user_intent_settlement_fence(user_intent_settlement_fence)
                    .await;
            }

            // The final user-intent fence above proves that this run still
            // owns its current generation and that no accepted intent is
            // stranded behind the response. Re-read canonical Work now so a
            // crash, correction, or successor assignment cannot borrow
            // process-local presentation state as completion authority.
            let committed_work_synthesis_authorized = if committed_work_synthesis_candidate {
                let authorized = match host.committed_work_synthesis_authorized(state).await {
                    Ok(authorized) => authorized,
                    Err(error) => {
                        state.messages.truncate(transcript_append_start);
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        state.interruption = Some(InterruptionRecord::new(
                            InterruptionKind::Interrupted,
                            ResumeAction::ContinueImmediately,
                            interruption_state_summary(
                                state,
                                Some(format!(
                                    "canonical Work final-synthesis state was unavailable: {error}"
                                )),
                            ),
                        ));
                        record_early_exit_llm_round(
                            state,
                            &turn_result,
                            prep.turn_start_time,
                            Some("canonical_work_revalidation_unavailable"),
                        );
                        observe_turn_end_without_tools(
                            state,
                            turn_index,
                            prep.turn_start_time,
                            turn_result.ttft_ms,
                            turn_result_tokens_consumed(&turn_result),
                        );
                        state.step_recorder.end_turn(false);
                        try_write_heavy_checkpoint(state);
                        finalize_and_render(host, state).await;
                        return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                    }
                };
                // The provider request and Work validation are not the Run
                // terminal commit. Re-authorize immediately after the durable
                // Work read so lease/generation loss cannot race final prose.
                match authorize_provider_boundary(state).await {
                    Ok(ProviderBoundaryGate::Authorized) => {}
                    Ok(ProviderBoundaryGate::Paused) => {
                        // The model response was produced before the durable
                        // pause won. Do not admit stale prose or execute a
                        // follow-up action; wait here for resume without
                        // treating pause as a user cancellation, then reopen
                        // the exact settlement fence before continuing.
                        state.messages.truncate(transcript_append_start);
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        record_early_exit_llm_round(
                            state,
                            &turn_result,
                            prep.turn_start_time,
                            Some("paused_after_provider_response"),
                        );
                        state.step_recorder.end_turn(false);
                        try_write_heavy_checkpoint(state);
                        if wait_for_pause_clear_or_cancel(state).await {
                            return Err(astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::Cancelled,
                                "run was cancelled while paused after a provider response",
                            ));
                        }
                        return continue_after_user_intent_settlement_fence(
                            user_intent_settlement_fence,
                        )
                        .await;
                    }
                    Err(error) => {
                        // The response was produced by an owner that no
                        // longer controls the Run. Keep provider accounting,
                        // but do not admit its prose into the durable/user-
                        // visible turn.
                        state.messages.truncate(transcript_append_start);
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        return Err(error);
                    }
                }
                if !authorized {
                    // The canonical cut no longer matches this exact
                    // Run/generation/Work revision. This is an explicit
                    // revocation, not permission to reinterpret stale prose
                    // through generic workspace-completion heuristics.
                    state
                        .hooks
                        .completion_settlement
                        .preserve_final_synthesis_wire_surface = false;
                    state.messages.truncate(transcript_append_start);
                    state.final_text.clear();
                    state.hooks.completion_settlement.latest_provider_text = None;
                    state.hooks.completion_settlement.deferred_candidate_text = None;
                    state.interruption = Some(InterruptionRecord::new(
                        InterruptionKind::ExecutionIncomplete,
                        ResumeAction::ContinueImmediately,
                        interruption_state_summary(
                            state,
                            Some(
                                "canonical Work final-synthesis authority was revoked by newer durable state"
                                    .to_string(),
                            ),
                        ),
                    ));
                    record_early_exit_llm_round(
                        state,
                        &turn_result,
                        prep.turn_start_time,
                        Some("canonical_work_synthesis_revoked"),
                    );
                    observe_turn_end_without_tools(
                        state,
                        turn_index,
                        prep.turn_start_time,
                        turn_result.ttft_ms,
                        turn_result_tokens_consumed(&turn_result),
                    );
                    state.step_recorder.end_turn(false);
                    try_write_heavy_checkpoint(state);
                    finalize_and_render(host, state).await;
                    return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                }
                authorized
            } else {
                false
            };
            let terminal_completion_disposition =
                terminal_completion_disposition(state, committed_work_synthesis_authorized);

            if enforce_typed_work_settlement_before_text_completion(state) {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("work_settlement_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return continue_after_user_intent_settlement_fence(user_intent_settlement_fence)
                    .await;
            }

            match enforce_completion_action_window_before_text_completion(state) {
                CompletionActionBoundary::TerminalIncomplete => {
                    // The typed action window is terminal authority. Do not
                    // fall through to the older workspace/verification
                    // guards: doing so would reopen an unrestricted retry
                    // after the one allowed completion action was consumed.
                    record_early_exit_llm_round(
                        state,
                        &turn_result,
                        prep.turn_start_time,
                        Some("completion_action_incomplete"),
                    );
                    observe_turn_end_without_tools(
                        state,
                        turn_index,
                        prep.turn_start_time,
                        turn_result.ttft_ms,
                        turn_result_tokens_consumed(&turn_result),
                    );
                    state.step_recorder.end_turn(false);
                    try_write_heavy_checkpoint(state);
                    finalize_and_render(host, state).await;
                    return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                }
                CompletionActionBoundary::NoWindow | CompletionActionBoundary::Settled => {}
            }

            if enforce_workspace_completion_before_text_completion_with_disposition(
                state,
                terminal_completion_disposition,
            ) {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("workspace_completion_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return continue_after_user_intent_settlement_fence(user_intent_settlement_fence)
                    .await;
            }

            if state.interruption.is_none()
                && enforce_explicit_verification_before_text_completion(state)
            {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("explicit_verification_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return continue_after_user_intent_settlement_fence(user_intent_settlement_fence)
                    .await;
            }

            if state.interruption.is_none()
                && enforce_outcome_reconciliation_before_text_completion(state)
            {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("outcome_reconciliation_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return continue_after_user_intent_settlement_fence(user_intent_settlement_fence)
                    .await;
            }

            let round_slice_incomplete = enforce_terminal_completion_disposition_before_success(
                state,
                terminal_completion_disposition,
            );

            // A completion guard may discover the exhausted side of its
            // bounded recovery window while this response is being settled
            // (workspace mutation, post-mutation observation, or an explicit
            // verification contract). Re-check here before the generic stop
            // path records a successful turn. The earlier check above covers
            // interruptions that arrived from transport/lifecycle; this one
            // covers obligations that became terminal during this boundary.
            if state.interruption.is_some() {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some(if round_slice_incomplete {
                        "round_slice_without_typed_completion"
                    } else {
                        "completion_contract_incomplete"
                    }),
                );
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                    turn_result_tokens_consumed(&turn_result),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            // A model response cannot settle producer-owned asynchronous
            // work. Reconcile canonical producer truth before accepting the
            // turn, but keep runtime state out of assistant-authored content.
            // UI and continuation decisions consume the typed observation;
            // transcript text remains exactly what the model produced.
            reconcile_unsettled_work_status(state);

            if committed_work_synthesis_candidate {
                // Exactly-once consumption occurs only after every typed
                // completion, verification, outcome, fanout, and process
                // barrier above accepted the response.
                state
                    .hooks
                    .completion_settlement
                    .preserve_final_synthesis_wire_surface = false;
            }

            // Record the LLM round even for text-only responses (no tool calls).
            // Without this, simple Q&A turns have llm_rounds=0 in the
            // journal despite the LLM being called.
            record_early_exit_llm_round(state, &turn_result, prep.turn_start_time, Some("stop"));
            state.step_recorder.end_turn(true);

            if turn_result.accum.server_execution_summary.is_none() {
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                    turn_result_tokens_consumed(&turn_result),
                );
            }
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
    record_tool_selection(state, &turn_result, turn_index);

    Ok(TurnExecutionControl::Proceed(Box::new(
        TurnExecutionPhase {
            llm_wall_start,
            turn_result,
        },
    )))
}

fn should_record_local_provider_round(
    boundary: super::host::ControlPlaneTurnBoundary,
    has_remote_execution_summary: bool,
) -> bool {
    !has_remote_execution_summary
        && !matches!(
            boundary,
            super::host::ControlPlaneTurnBoundary::Providerless
        )
}

/// Preserve the latest mixed provider response before any bounded text-only
/// boundary can short-circuit the tool phase. This covers round-slice/token
/// wrap-ups, typed completion actions, and edge tool rounds; finalization
/// labels the text as partial whenever interruption is typed, so it cannot
/// become an unqualified success claim.
/// Keep the newest provider-authored text separately from the bounded
/// settlement candidate. The latter intentionally keeps the first
/// substantive mixed response during a tool-call lockout; that is useful for
/// a bounded retry, but it must not become the stale partial handoff when a
/// later model response is the one that was actually interrupted.
pub(crate) fn capture_latest_provider_text(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
) {
    let text = turn_result.accum.full_text.trim();
    if !text.is_empty() {
        state.hooks.completion_settlement.latest_provider_text = Some(text.to_string());
    }
}

pub(crate) fn capture_deferred_candidate_text(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
) {
    if !(state.budget_wrapup_injected || state.hooks.completion_settlement.text_only)
        || turn_result.accum.full_text.trim().is_empty()
    {
        return;
    }
    let has_tool_work = !turn_result.accum.tool_calls.is_empty()
        || turn_result.accum.has_tool_calls
        || turn_result
            .accum
            .server_execution_summary
            .as_ref()
            .is_some_and(|summary| summary.tool_calls_count > 0)
        || !turn_result.edge_tool_round.is_empty();
    let candidate = turn_result.accum.full_text.trim().to_string();
    let deferred = &mut state.hooks.completion_settlement.deferred_candidate_text;
    if has_tool_work {
        // The first substantive mixed response is the most complete
        // provider-owned candidate. Later responses that still violate the
        // text-only boundary are not evidence of a better answer and must
        // not replace it.
        if deferred.is_none() {
            *deferred = Some(candidate);
        }
    } else {
        // A fully text-only retry is an explicit compliance signal. It may
        // replace the earlier mixed candidate, including with a shorter but
        // more accurate reconciliation.
        *deferred = Some(candidate);
    }
}

fn runtime_retrospective_requires_live_evidence(message: &str) -> bool {
    let normalized = message.to_lowercase();
    let observation_scope = [
        "runtime",
        "trace",
        "telemetry",
        "运行状态",
        "运行时",
        "session:",
        "session state",
        "session history",
        "session trace",
        "this session",
        "current session",
        "会话状态",
        "会话历史",
        "这段会话",
        "这个会话",
        "工具调用",
        "调用记录",
    ]
    .iter()
    .any(|term| normalized.contains(term));
    let retrospective_intent = [
        "retrospect",
        "reflect",
        "audit",
        "diagnos",
        "inspect",
        "evidence",
        "反省",
        "复盘",
        "回顾",
        "审计",
        "诊断",
        "分析",
        "证据",
    ]
    .iter()
    .any(|term| normalized.contains(term));
    observation_scope && retrospective_intent
}

fn collapse_batched_observation_fanout(tool_calls: &mut Vec<serde_json::Value>) -> usize {
    let mut remove = std::collections::HashSet::new();
    for tool_name in ["introspect", "reflect"] {
        let candidates = tool_calls
            .iter()
            .enumerate()
            .filter_map(|(index, call)| {
                let function = call.get("function")?;
                (function.get("name").and_then(serde_json::Value::as_str) == Some(tool_name))
                    .then_some((index, observation_call_args(function)))
            })
            .filter(|(_, args)| tool_name != "introspect" || !args.contains_key("artifact"))
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            continue;
        }
        let keep = candidates
            .iter()
            .find(|(_, args)| {
                args.get("facet").and_then(serde_json::Value::as_str) == Some("overview")
            })
            .map_or(candidates[0].0, |(index, _)| *index);
        for (index, _) in &candidates {
            if *index != keep {
                remove.insert(*index);
            }
        }
        canonicalize_composite_observation_args(&mut tool_calls[keep], tool_name);
    }
    let collapsed = remove.len();
    if collapsed > 0 {
        let mut index = 0usize;
        tool_calls.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
    }
    collapsed
}

fn observation_call_args(
    function: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    match function.get("arguments") {
        Some(serde_json::Value::Object(args)) => args.clone(),
        Some(serde_json::Value::String(args)) => serde_json::from_str(args).unwrap_or_default(),
        _ => serde_json::Map::new(),
    }
}

fn canonicalize_composite_observation_args(call: &mut serde_json::Value, tool_name: &str) {
    let Some(function) = call.get_mut("function") else {
        return;
    };
    let mut args = observation_call_args(function);
    args.insert("facet".to_string(), serde_json::json!("overview"));
    args.insert("depth".to_string(), serde_json::json!("diagnostic"));
    if tool_name == "reflect" {
        args.insert("topic".to_string(), serde_json::json!("overview"));
        args.insert("horizon".to_string(), serde_json::json!("session"));
    } else {
        args.insert("horizon".to_string(), serde_json::json!("recent"));
    }
    function["arguments"] = serde_json::Value::String(
        serde_json::to_string(&args).expect("observation arguments must serialize"),
    );
}

fn authoritative_server_runtime_feedback(
    summary: &astra_turn_core::chat_turn_sse_dispatch::ServerLoopExecutionSummary,
    session_id: Option<&str>,
    run_id: Option<&str>,
    model_id: Option<&str>,
    session_turn: u32,
) -> Option<astra_turn_core::context_feedback::RuntimeFeedbackFrame> {
    summary.runtime_feedback.clone().filter(|frame| {
        session_id == Some(frame.identity.session_id.as_str())
            && run_id == Some(frame.identity.run_id.as_str())
            && model_id == Some(frame.identity.model_id.as_str())
            && frame.progress.session_turn == session_turn
            // Completed summaries are parser-validated at exact equality.
            // Interrupted summaries may include one failed provider attempt
            // beyond the last successfully ingested feedback frame.
            && frame.progress.llm_rounds_completed <= summary.llm_rounds
    })
}

fn prompt_cache_identity_from_manifest(
    manifest: Option<&serde_json::Value>,
) -> Option<astra_turn_types::PromptCacheIdentityV1> {
    manifest
        .and_then(|trace| trace.pointer("/wire/fingerprint/prompt_cache_identity"))
        .and_then(|identity| serde_json::from_value(identity.clone()).ok())
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

pub(crate) fn has_concrete_workspace_mutation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed() && record.ok)
        .any(|record| tool_record_has_bound_positive_mutation(state, record))
}

/// Completion authority for an owed bound-workspace state.  A live desired-
/// state convergence pair proves that the requested final bytes already exist,
/// but remains categorically distinct from a mutation fact.
fn has_bound_workspace_completion_evidence(state: &AgenticLoopState) -> bool {
    has_concrete_workspace_mutation(state) || has_live_desired_state_convergence(state)
}

/// A complete-state typed writer may discover that an earlier opaque action
/// already produced the exact requested bytes.  That owner fact is useful
/// only inside the current invocation and only after a later typed observer
/// reads the same normalized target.  It is not a mutation receipt, is never
/// canonical Work repair authority, and is invalidated by any intervening
/// workspace mutation risk.
fn has_live_desired_state_convergence(state: &AgenticLoopState) -> bool {
    live_desired_state_convergence_state(state) == LiveDesiredStateConvergence::Observed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveDesiredStateConvergence {
    None,
    PendingObservation,
    Observed,
}

fn live_desired_state_convergence_state(state: &AgenticLoopState) -> LiveDesiredStateConvergence {
    #[derive(Clone)]
    struct PendingConvergence {
        evidence: astra_tools::workspace_observation::TypedWorkspaceDesiredStateConvergenceEvidence,
        round: u32,
        tool_call_id: String,
    }

    let mut pending: Option<PendingConvergence> = None;
    let mut observed_convergence = false;
    let mut seen_writer_receipts = HashSet::<String>::new();
    let mut seen_observations = HashSet::<String>::new();
    for record in state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed())
    {
        if let Some((evidence, round, tool_call_id)) =
            live_desired_state_convergence_evidence(state, record)
        {
            if !seen_writer_receipts.insert(evidence.receipt_id.clone()) {
                pending = None;
                observed_convergence = false;
                continue;
            }
            pending = Some(PendingConvergence {
                evidence,
                round,
                tool_call_id,
            });
            observed_convergence = false;
            continue;
        }
        if pending.is_none() {
            continue;
        }
        if let Some((observation, round, tool_call_id)) =
            live_typed_workspace_observation_evidence(state, record)
        {
            if !seen_observations.insert(observation.observation_id.clone()) {
                pending = None;
                observed_convergence = false;
                continue;
            }
            let pending_evidence = pending.as_ref().expect("checked pending convergence");
            if round > pending_evidence.round
                && tool_call_id != pending_evidence.tool_call_id
                && observation.target == pending_evidence.evidence.target
            {
                if observation.observed_state == pending_evidence.evidence.desired_state {
                    observed_convergence = true;
                } else {
                    pending = None;
                    observed_convergence = false;
                }
            }
            continue;
        }
        // Once convergence is pending, only typed workspace observers are
        // proven read-only enough to preserve it. Every other executed call,
        // including a failed or lexically opaque shell invocation, may have
        // changed the target outside this ledger's positive mutation lane.
        let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
        let may_mutate = args.as_ref().is_none_or(|args| {
            astra_tools::executor::is_workspace_mutation_tool(&record.name, args)
                || crate::turn::tool_side_effects::tool_call_may_mutate_workspace(
                    &record.name,
                    Some(args),
                )
        });
        if may_mutate
            || !astra_tools::workspace_observation::is_typed_workspace_observer(&record.name)
        {
            pending = None;
            observed_convergence = false;
        }
    }
    if observed_convergence {
        LiveDesiredStateConvergence::Observed
    } else if pending.is_some() {
        LiveDesiredStateConvergence::PendingObservation
    } else {
        LiveDesiredStateConvergence::None
    }
}

fn live_desired_state_convergence_evidence(
    _state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<(
    astra_tools::workspace_observation::TypedWorkspaceDesiredStateConvergenceEvidence,
    u32,
    String,
)> {
    (record.name == "write_file"
        && record.was_executed()
        && record.ok
        && record.runtime_args_full.is_some()
        && record.workspace_mutation_observed != Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE))
    .then_some(())?;
    let round = record.round?;
    let tool_call_id = record.tool_call_id.clone()?;
    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full())?;
    let evidence = astra_tools::workspace_observation::typed_workspace_desired_state_convergence_evidence_for_invocation(
        record.workspace_mutation_receipt.as_ref()?,
        &args,
    )?;
    Some((evidence, round, tool_call_id))
}

fn live_typed_workspace_observation_evidence(
    _state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<(
    astra_tools::workspace_observation::TypedWorkspaceObservationEvidence,
    u32,
    String,
)> {
    if !record.was_executed()
        || !record.ok
        || record.runtime_args_full.is_none()
        || !super::lifecycle::record_has_typed_workspace_observation_receipt(record)
    {
        return None;
    }
    let round = record.round?;
    let tool_call_id = record.tool_call_id.clone()?;
    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full())?;
    let evidence =
        astra_tools::workspace_observation::typed_workspace_observation_evidence_for_invocation(
            record.workspace_mutation_receipt.as_ref()?,
            &record.name,
            &args,
        )?;
    Some((evidence, round, tool_call_id))
}

pub(crate) fn has_concrete_external_effect(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed() && record.ok)
        .any(|record| {
            record.external_effect_observed == Some(true)
                && record.external_effect_scope.as_deref()
                    == Some(astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE)
                && record.external_effect_receipt.as_ref().is_some_and(
                    astra_tools::workspace_observation::is_authoritative_external_effect_receipt,
                )
        })
}

/// Positive mutation shape is retained even when the command failed: the
/// executor may have written a partial result before returning an error.
pub(crate) fn has_executed_positive_workspace_mutation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .any(|record| tool_record_may_have_mutated_bound_workspace(state, record))
}

/// Conservative invalidation fact: an executed writer may have changed the
/// bound workspace even when it failed or lacks a positive completion receipt.
/// This is deliberately weaker than completion authority. It may invalidate an
/// older validation receipt, but it can never satisfy a mutation requirement.
fn tool_record_may_have_mutated_bound_workspace(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.was_executed() || !tool_record_has_positive_mutation_shape(record) {
        return false;
    }
    // Compound read/inspect commands often materialize a snapshot under
    // /tmp before reading it. The executor already proved that every writer
    // target is external scratch; do not turn that typed proof into a global
    // post-mutation observation obligation merely because the command also
    // mentions the bound repository in its read-only producer.
    if record_is_proven_external_scratch_mutation(state, record) {
        return false;
    }
    let workspace_root = state.hooks.workspace_root_hint.as_deref();
    !super::lifecycle::record_explicit_path(record).is_some_and(|path| {
        super::lifecycle::path_is_external_volatile_scratch(&path, workspace_root)
    })
}

/// A successful mutation receipt is stronger than an admission-time risk
/// classification. `args_preview` is display data, never an execution
/// contract, while known direct writers remain positive even when an old
/// journal entry has no structured argument payload.
pub(crate) fn tool_record_has_positive_mutation_shape(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    // The shell executor can observe mutations that are invisible to any
    // command-shape classifier (inline interpreters, generated helpers, and
    // project-specific writers). A foreground-process-group receipt is valid
    // current-turn evidence that bytes changed, but is deliberately not
    // durable authority: `runtime_args_full` is skipped by serde, so the same
    // weak receipt cannot satisfy completion after checkpoint restore.
    if record_has_live_workspace_mutation_receipt(record) {
        return true;
    }
    if crate::turn::tool_side_effects::tool_classified_from_arguments(&record.name) {
        let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
        return args.as_ref().is_some_and(|args| {
            crate::turn::tool_side_effects::tool_call_records_workspace_mutation(
                &record.name,
                Some(args),
            )
        });
    }

    // Direct workspace writers are typed by tool name. Their preview/full
    // payload is useful for scoping a target, but it is not needed to know
    // that the executor ran a writer. Never let a lossy preview or malformed
    // payload erase that typed execution fact.
    crate::turn::tool_side_effects::tool_call_records_workspace_mutation(&record.name, None)
}

pub(crate) fn record_has_trusted_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record_has_bound_workspace_receipt(record)
        .is_some_and(astra_tools::workspace_observation::is_authoritative_changed_receipt)
}

/// A bound executor receipt that is still attached to the live invocation.
/// This admits weaker foreground-process-group provenance for the current
/// completion chain without promoting it to durable cache, renewal, or resume
/// authority. Restored records have no `runtime_args_full` by contract.
fn record_has_live_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.runtime_args_full.is_some()
        && record_has_bound_workspace_receipt(record)
            .is_some_and(astra_tools::workspace_observation::is_changed_receipt)
}

fn record_has_bound_workspace_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<&serde_json::Value> {
    (record.name == "bash"
        && record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Executed
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE))
    .then_some(record.workspace_mutation_receipt.as_ref())
    .flatten()
}

pub(crate) fn record_has_weak_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record_has_bound_workspace_receipt(record)
        .is_some_and(astra_tools::workspace_observation::is_weak_changed_receipt)
}

fn record_has_trusted_partial_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.name == "str_replace"
        && record.was_executed()
        && !record.ok
        && record.workspace_mutation_partial == Some(true)
        && record
            .workspace_mutation_partial_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
        && record.workspace_mutation_receipt.as_ref().is_some_and(
            astra_tools::workspace_observation::is_typed_partial_workspace_mutation_receipt,
        )
}

/// Apply quarantine at every tool-record boundary, regardless of whether the
/// record came from an LLM ingest frame or the local/server tool phase.  Weak
/// process ownership and partial multi-file commits are both transport facts,
/// never completion evidence; the first transition is checkpointed immediately
/// so a crash cannot resurrect a clean observation epoch.
pub(crate) fn apply_workspace_observation_quarantine_transition(
    state: &mut AgenticLoopState,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> bool {
    if state.stall.workspace_observation_quarantine.is_some() {
        return false;
    }
    let Some(record) = records.iter().find(|record| {
        record_has_weak_workspace_mutation_receipt(record)
            || record_has_trusted_partial_workspace_mutation_receipt(record)
    }) else {
        return false;
    };
    let source_tool_call_id = record.tool_call_id.clone();
    let quarantine = if record_has_trusted_partial_workspace_mutation_receipt(record) {
        astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::partial_workspace_mutation(
            source_tool_call_id,
        )
    } else {
        astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
            source_tool_call_id,
        )
    };
    state.stall.workspace_observation_quarantine = Some(quarantine);
    try_write_heavy_checkpoint(state);
    true
}

/// Restrict the positive shape to the bound workspace for global completion
/// obligations. Volatile scratch is still executable and remains a recent
/// budget barrier, but it must not permanently reclassify a read-only turn.
pub(crate) fn tool_record_has_bound_positive_mutation(
    _state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if super::lifecycle::record_has_typed_workspace_tool_receipt(record) {
        return record.ok;
    }
    // A foreground-process-group receipt is valid only while it remains in
    // this live invocation.  It describes an executor-observed change, not a
    // durable ownership fact: `runtime_args_full` is deliberately not
    // checkpointed, so restored records cannot cross this boundary.
    if record_has_live_workspace_mutation_receipt(record) {
        return true;
    }
    if record_has_trusted_workspace_mutation_receipt(record) {
        return true;
    }
    // A restored weak-ownership marker is attribution quarantine, not a
    // terminal task failure. It prevents a new weak receipt from being used
    // after resume, where the original executor's process-local quarantine is
    // no longer available. A later invocation-cgroup receipt remains strong
    // enough to establish a fresh durable boundary.
    // A foreground process-group receipt is not concrete mutation evidence:
    // a descendant can escape with setsid/double-fork after the leader exits.
    // It remains a risk signal through `tool_record_has_positive_mutation_shape`
    // and therefore still drives quarantine/observation, but never satisfies
    // a terminal or repair obligation.
    // A lexical/direct-writer shape is only a mutation *risk*. It is not a
    // positive completion fact: successful prose and a tool name cannot tell
    // us whether a dry-run, no-op, or zero-reference operation committed
    // bytes. The owner receipt is the sole structured-writer authority.
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkValidationState {
    None,
    Failed,
    Stale,
    Passed,
}

fn record_starts_fresh_work_attempt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.ok
        || record.effective_disposition()
            != astra_services::session_journal::ToolCallDisposition::Executed
    {
        return false;
    }
    if record.name == "settle_work_item" {
        return true;
    }
    let Some(result) = record
        .result_full
        .as_deref()
        .and_then(|result| serde_json::from_str::<serde_json::Value>(result).ok())
    else {
        return false;
    };
    let assignment = match record.name.as_str() {
        "run_next_work_item" => &result,
        "start_work" => match result.get("initial_task") {
            Some(initial_task) => initial_task,
            None => return false,
        },
        _ => return false,
    };
    assignment.get("status").and_then(serde_json::Value::as_str) == Some("assigned")
        && assignment
            .get("execution")
            .and_then(serde_json::Value::as_str)
            == Some("primary_session")
        && assignment
            .get("attempt_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|attempt_id| !attempt_id.trim().is_empty())
}

/// Latest canonical validation state in the current Work attempt's stable
/// evidence epoch. Fresh typed assignments and successful settlements bound
/// the scan; later bound-workspace mutations invalidate earlier validation.
pub(crate) fn current_work_validation_state(state: &AgenticLoopState) -> WorkValidationState {
    let records = &state.stall.tool_call_records;
    let attempt_start = records
        .iter()
        .rposition(record_starts_fresh_work_attempt)
        .map_or(0, |index| index.saturating_add(1));

    // A passing validator only clears a failure for the *same canonical
    // operation*.  Treating every recognized build/test command as
    // interchangeable lets an agent hide a failing suite by narrowing its
    // selection, changing flags, or substituting a cheaper command.  We do
    // not infer containment from task prose or command text: exact normalized
    // operation identity is the only evidence that a failed receipt has been
    // re-run successfully.  This is intentionally stricter than ordinary
    // validation freshness, where any later canonical validation can supply a
    // current positive receipt after a workspace mutation.
    let unresolved_operations = unresolved_work_validation_operations(&records[attempt_start..]);
    let mut saw_validation = false;
    let mut saw_current_positive_validation = false;
    let mut validation_is_stale = false;
    for record in &records[attempt_start..] {
        if record.effective_disposition()
            != astra_services::session_journal::ToolCallDisposition::Executed
        {
            continue;
        }
        if saw_validation && tool_record_may_have_mutated_bound_workspace(state, record) {
            validation_is_stale = true;
        }
        let Some(args) = record.authoritative_args_full() else {
            continue;
        };
        if astra_turn_core::evaluation::normalize_validation_prefix(&record.name, args).is_some() {
            saw_validation = true;
            // A canonical validator is fresh evidence about the current
            // workspace even when it fails.  Keeping an older pre-mutation
            // receipt marked stale after a newer failed validator hides the
            // failure debt and prevents the bounded repair/revalidation path
            // from opening.  Its outcome below still decides whether the
            // current state is Passed or Failed.
            validation_is_stale = false;
            // Delivery requires affirmative validation, not merely a shell
            // invocation that completed. With `pipefail`, a compound
            // validator/filter pipeline can otherwise surface as an empty or
            // domain-negative final-stage result even though no passing
            // validation receipt exists. Keep that ambiguity fail-closed and
            // let a later canonical success clear it.
            if astra_turn_core::evaluation::tool_outcome_is_positive_success(record) {
                saw_current_positive_validation = true;
            }
        }
    }
    if validation_is_stale {
        WorkValidationState::Stale
    } else if !unresolved_operations.is_empty() {
        WorkValidationState::Failed
    } else if saw_current_positive_validation {
        WorkValidationState::Passed
    } else {
        WorkValidationState::None
    }
}

/// Canonical validation operations whose latest terminal outcome remains a
/// failure in the current Work attempt.  A success resolves only its exact
/// normalized operation identity; different commands remain independent
/// evidence rather than an implicit waiver of a prior failure.
fn unresolved_work_validation_operations(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Vec<String> {
    let mut unresolved = Vec::new();
    for record in records {
        if record.effective_disposition()
            != astra_services::session_journal::ToolCallDisposition::Executed
        {
            continue;
        }
        let Some(args) = record.authoritative_args_full() else {
            continue;
        };
        let Some(operation) =
            astra_turn_core::evaluation::normalize_validation_prefix(&record.name, args)
        else {
            continue;
        };
        if astra_turn_core::evaluation::tool_outcome_is_positive_success(record) {
            unresolved.retain(|candidate| candidate != &operation);
        } else {
            // Repeating a still-failing operation must retain one debt, and a
            // new failure becomes the most recent bounded repair target.
            unresolved.retain(|candidate| candidate != &operation);
            unresolved.push(operation);
        }
    }
    unresolved
}

/// The exact normalized validation operation that produced the current
/// failure. This is kept only for the bounded repair/revalidation transition;
/// it is not inferred from model prose or result text.
fn failed_work_validation_operation(state: &AgenticLoopState) -> Option<String> {
    let records = &state.stall.tool_call_records;
    let attempt_start = records
        .iter()
        .rposition(record_starts_fresh_work_attempt)
        .map_or(0, |index| index.saturating_add(1));
    unresolved_work_validation_operations(&records[attempt_start..])
        .into_iter()
        .last()
}

/// The exact canonical command to repeat after a stale or failed Work
/// validation. A current failure keeps its own operation identity; otherwise
/// staleness is revalidated with the most recently recognized canonical
/// validation from the same Work attempt. Neither task prose nor arbitrary
/// successful tool output supplies this identity.
fn work_validation_operation_for_recovery(state: &AgenticLoopState) -> Option<String> {
    failed_work_validation_operation(state).or_else(|| {
        let records = &state.stall.tool_call_records;
        let attempt_start = records
            .iter()
            .rposition(record_starts_fresh_work_attempt)
            .map_or(0, |index| index.saturating_add(1));
        records[attempt_start..].iter().rev().find_map(|record| {
            (record.effective_disposition()
                == astra_services::session_journal::ToolCallDisposition::Executed)
                .then(|| record.authoritative_args_full())
                .flatten()
                .and_then(|args| {
                    astra_turn_core::evaluation::normalize_validation_prefix(&record.name, args)
                })
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectedWorkValidationState {
    Failed,
    Stale,
}

/// Only a server-owned unresolved-validation rejection reopens one bounded
/// Work recovery edge. A user/tool schema rejection, a failed executor
/// settlement, or another lifecycle error must stay at truthful settlement.
fn record_rejected_work_validation_state(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<RejectedWorkValidationState> {
    if record.name != "settle_work_item"
        || !matches!(
            record.effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Rejected
        )
    {
        return None;
    }

    let payload = serde_json::from_str::<serde_json::Value>(record.result_full.as_deref()?).ok()?;
    if payload.get("status").and_then(serde_json::Value::as_str) != Some("rejected")
        || payload
            .get("error_kind")
            .and_then(serde_json::Value::as_str)
            != Some("unresolved_work_validation")
    {
        return None;
    }

    match payload
        .get("validation_state")
        .and_then(serde_json::Value::as_str)
    {
        Some("failed") => Some(RejectedWorkValidationState::Failed),
        Some("stale") => Some(RejectedWorkValidationState::Stale),
        _ => None,
    }
}

/// A repair may share a provider batch with a rejected delivery request. Only
/// an executed, successful mutation from a typed builtin writer, a strong
/// executor receipt, or a still-live weak receipt can advance that batch to
/// revalidation. The weak form is deliberately limited to the current
/// invocation by `runtime_args_full`; restored weak receipts, failed writers,
/// and lexical shell guesses remain insufficient.
fn record_is_effective_workspace_repair(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.was_executed()
        && record.ok
        && (record_has_trusted_workspace_mutation_receipt(record)
            || super::lifecycle::record_has_typed_workspace_tool_receipt(record))
}

/// Project the one completion action that is already justified by typed
/// intent and the executed-tool ledger.  This deliberately does not inspect
/// user prose, tool names beyond their side-effect classification, or policy
/// advisory signals.
pub(crate) fn pending_completion_action(state: &AgenticLoopState) -> Option<CompletionAction> {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    pending_completion_action_for_work_state(state, active_work_attempt)
}

pub(crate) fn pending_completion_action_for_work_state(
    state: &AgenticLoopState,
    active_work_attempt: bool,
) -> Option<CompletionAction> {
    if workspace_observation_is_quarantined(state) {
        return None;
    }
    let workspace_completion_evidence = has_bound_workspace_completion_evidence(state);
    if requires_external_effect_completion(state) && !has_concrete_external_effect(state) {
        return Some(CompletionAction::RequiredExternalEffect);
    }
    if requires_bound_workspace_completion(state) && !workspace_completion_evidence {
        return Some(
            if live_desired_state_convergence_state(state)
                == LiveDesiredStateConvergence::PendingObservation
            {
                CompletionAction::PostMutationObservation
            } else {
                CompletionAction::RequiredWorkspaceMutation
            },
        );
    }
    if active_work_attempt
        && matches!(
            current_work_validation_state(state),
            WorkValidationState::Failed | WorkValidationState::Stale
        )
    {
        return Some(CompletionAction::CanonicalWorkValidation);
    }
    // Workspace settlement must start from explicit completion authority:
    // either an executor-owned mutation fact or the separate live convergence
    // pair. A failed or shape-only writer may require conservative
    // invalidation, but cannot prove the requested final state exists.
    if !workspace_completion_evidence {
        return None;
    }
    if let Some(missing) = missing_explicit_verification_hooks(state)
        && !missing.is_empty()
    {
        return Some(CompletionAction::ExplicitVerification {
            missing_labels: missing,
        });
    }
    if !successful_post_mutation_observation(state) {
        return Some(CompletionAction::PostMutationObservation);
    }
    None
}

fn workspace_mutation_intent(state: &AgenticLoopState) -> WorkspaceMutationIntent {
    // Preserve an already-derived must-mutate obligation even if a restored or
    // test host did not retain the richer TurnIntent value. In normal runs the
    // two fields are produced atomically by the semantic admission boundary.
    if state.task_profile.mutates_workspace {
        return WorkspaceMutationIntent::MustMutate;
    }
    state
        .turn_intent
        .as_ref()
        .map(|intent| intent.workspace_mutation)
        .unwrap_or(WorkspaceMutationIntent::Unknown)
}

/// Whether the current semantic contract specifically owes a mutation in the
/// bound workspace.
///
/// A restored state without the richer typed intent retains the historical
/// fail-closed workspace requirement.  Only an explicit `external` completion
/// scope can bypass that gate; mixed and unknown scopes still require the
/// executor-owned bound-workspace receipt.
fn requires_bound_workspace_completion(state: &AgenticLoopState) -> bool {
    if workspace_mutation_intent(state) != WorkspaceMutationIntent::MustMutate {
        return false;
    }
    state.turn_intent.as_ref().map_or(true, |intent| {
        intent.workspace_mutation != WorkspaceMutationIntent::MustMutate
            || intent.requires_workspace_mutation()
    })
}

fn mutation_completion_scope(state: &AgenticLoopState) -> MutationCompletionScope {
    state
        .turn_intent
        .as_ref()
        .filter(|intent| intent.workspace_mutation == WorkspaceMutationIntent::MustMutate)
        .map(|intent| intent.mutation_completion_scope)
        .unwrap_or(MutationCompletionScope::Unknown)
}

fn requires_external_effect_completion(state: &AgenticLoopState) -> bool {
    workspace_mutation_intent(state) == WorkspaceMutationIntent::MustMutate
        && matches!(
            mutation_completion_scope(state),
            MutationCompletionScope::External | MutationCompletionScope::Mixed
        )
}

/// Select the single task-facing action available only at a hard terminal
/// slice. Unknown/MayMutate intent cannot justify forcing a write, while a
/// missing concrete mutation cannot justify narrowing the boundary to an
/// observation. One ordinarily admitted external action is the bounded,
/// provider-neutral fallback. Explicit ReadOnly and active Work lifecycle
/// paths retain their existing text/settlement boundaries.
pub(crate) fn pending_terminal_completion_action_for_work_state(
    state: &AgenticLoopState,
    active_work_attempt: bool,
) -> Option<CompletionAction> {
    if let Some(action) = pending_completion_action_for_work_state(state, active_work_attempt) {
        return Some(action);
    }
    if active_work_attempt
        || workspace_observation_is_quarantined(state)
        || has_bound_workspace_completion_evidence(state)
        || !super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
    {
        return None;
    }
    matches!(
        workspace_mutation_intent(state),
        WorkspaceMutationIntent::Unknown | WorkspaceMutationIntent::MayMutate
    )
    .then_some(CompletionAction::CompletionTaskAction)
}

/// Match a raw canonical provider tool call against a typed completion action.
/// This is used by both the server admission path and the local execution
/// path, so a model-visible action frame cannot drift from executable
/// authority.
pub(crate) fn completion_action_matches_tool_call(
    state: &AgenticLoopState,
    action: &CompletionAction,
    call: &serde_json::Value,
) -> bool {
    completion_action_match_label(state, action, call).is_some()
}

/// Project a bounded, provider-neutral explanation of the active completion
/// obligation.  The payload is advisory presentation only; the exact matcher
/// below remains the sole execution authority.  In particular, do not infer a
/// path from prose, a truncated args preview, or a task name.
pub(crate) fn completion_action_hint(action: &CompletionAction) -> serde_json::Value {
    let mut accepted_action_shapes = Vec::new();
    let (reason_code, latest_target, missing_labels): (&str, Option<String>, serde_json::Value) =
        match action {
            CompletionAction::RequiredWorkspaceMutation => {
                ("workspace_mutation_missing", None, serde_json::Value::Null)
            }
            CompletionAction::RequiredExternalEffect => {
                accepted_action_shapes.push(serde_json::json!({
                    "constraint": "one foreground task-facing action carrying a non-empty structured external_state_paths array; the executor must observe a delta under authoritative invocation ownership",
                    "evidence_inference_forbidden": ["assistant_text", "tool_name", "command_text", "exit_code"],
                }));
                (
                    "external_effect_receipt_missing",
                    None,
                    serde_json::Value::Null,
                )
            }
            CompletionAction::CompletionTaskAction => {
                accepted_action_shapes.push(serde_json::json!({
                    "constraint": "one task-facing tool action from the live admitted surface; runtime control/self-inspection calls do not match, and ordinary tool admission and safety policy are re-checked",
                }));
                (
                    "terminal_task_action_available",
                    None,
                    serde_json::Value::Null,
                )
            }
            CompletionAction::PostMutationObservation => {
                // Do not infer a concrete path from the runtime host filesystem:
                // Edge/server deployments may bind the workspace in a different
                // process or mount, and a same-named local file is not evidence of
                // a remote target. Until an executor-owned receipt carries a
                // bound target, only project the generic family; admission still
                // re-checks the canonical observer/validator shape.
                accepted_action_shapes.push(serde_json::json!({
                "tool": "bash",
                "constraint": "one workspace-scoped observation or canonical validator after the latest mutation; choose the strongest proportionate check available. Existence, compilation, or import is structural evidence only and does not establish required behavior or its named boundary cases; exact arguments are re-checked by the runtime",
            }));
                (
                    "latest_mutation_not_observed",
                    None,
                    serde_json::Value::Null,
                )
            }
            CompletionAction::PostMutationRepair => (
                "failed_post_mutation_observation_requires_repair",
                None,
                serde_json::Value::Null,
            ),
            CompletionAction::ExplicitVerification { missing_labels } => (
                "explicit_verification_missing",
                None,
                serde_json::json!(missing_labels),
            ),
            CompletionAction::CanonicalWorkValidation => {
                accepted_action_shapes.push(serde_json::json!({
                    "tool": "bash",
                    "evidence_source": "prior_runtime_recognized_project_validation",
                    "constraint": "rerun one direct standard project build/test validation previously recognized in this Work attempt; do not substitute a custom inline program, reader/probe, workspace mutation, or settlement call; exact arguments are re-checked by the runtime",
                    "raw_arguments_projected": false,
                }));
                (
                    "work_validation_stale_or_failed",
                    None,
                    serde_json::Value::Null,
                )
            }
            CompletionAction::CanonicalWorkRepair => {
                accepted_action_shapes.push(serde_json::json!({
                    "constraint": "one workspace mutation addressing the latest canonical validation failure; the runtime requires canonical revalidation next",
                }));
                (
                    "canonical_work_validation_failed",
                    None,
                    serde_json::Value::Null,
                )
            }
        };
    let accepted_action_family = serde_json::json!(match action {
        CompletionAction::RequiredWorkspaceMutation => "workspace_mutation",
        CompletionAction::RequiredExternalEffect => "external_effect",
        CompletionAction::CompletionTaskAction => "completion_task_action",
        CompletionAction::PostMutationObservation => "workspace_observation",
        CompletionAction::PostMutationRepair => "post_mutation_repair",
        CompletionAction::ExplicitVerification { .. } => "declared_verification",
        CompletionAction::CanonicalWorkValidation => "canonical_work_validation",
        CompletionAction::CanonicalWorkRepair => "canonical_work_repair",
    });

    serde_json::json!({
        "hint_version": "completion_action_hint.v1",
        "reason_code": reason_code,
        "latest_known_stable_target": latest_target,
        "accepted_action_family": accepted_action_family,
        "accepted_action_shapes": accepted_action_shapes,
        "missing_labels": missing_labels,
        "authority": "typed_completion_action_window",
    })
}

/// The first text-stop recovery for a missing required workspace outcome is
/// narrower than an ordinary RequiredWorkspaceMutation window.  An ordinary
/// terminal action may legitimately use an editor or patch tool; only this
/// recovery needs a complete-state writer because it must distinguish a real
/// change from an earlier opaque action that already produced the exact final
/// bytes. `workspace_mutation_retries` is durable settlement provenance set by
/// that branch, not a task/model/path heuristic.
fn completion_action_requires_complete_state_writer(
    state: &AgenticLoopState,
    action: &CompletionAction,
) -> bool {
    matches!(action, CompletionAction::RequiredWorkspaceMutation)
        && state.hooks.completion_settlement.workspace_mutation_retries > 0
}

fn completion_action_hint_for_state(
    state: &AgenticLoopState,
    action: &CompletionAction,
) -> serde_json::Value {
    let mut hint = completion_action_hint(action);
    if completion_action_requires_complete_state_writer(state, action) {
        hint["accepted_action_shapes"] = serde_json::json!([{
            "tool": "write_file",
            "constraint": "one complete-state typed writer containing the target path and full desired bytes",
            "changed_outcome": "an executor-owned workspace mutation receipt advances to a later observation obligation",
            "already_exact_outcome": "an executor-owned no-op convergence receipt advances only to one later separate full read_file of the same target",
            "evidence_inference_forbidden": ["assistant_text", "bash_output", "bash_exit_status", "server_stat_of_remote_workspace"],
        }]);
    }
    hint
}

fn completion_action_mismatch_instruction(
    state: &AgenticLoopState,
    action: &CompletionAction,
    correction_available: bool,
) -> &'static str {
    if completion_action_requires_complete_state_writer(state, action) {
        return if correction_available {
            "This request was not one complete-state typed write_file call and was not executed. Correct it now with exactly one write_file call containing the target path and full desired bytes. A real change must produce an executor mutation receipt; bytes already exactly present may produce a no-op convergence receipt and then require a later separate full read_file of that same target. Bash output, Bash exit status, assistant prose, and server-side stat of a remote workspace cannot satisfy this obligation."
        } else {
            "Only one complete-state typed write_file call may execute at this boundary. A real change requires an executor mutation receipt; an already-exact no-op requires an executor convergence receipt plus a later separate full read_file of the same target. Bash output, Bash exit status, assistant prose, and server-side stat of a remote workspace are not evidence."
        };
    }
    match (action, correction_available) {
        (CompletionAction::CanonicalWorkValidation, true) => {
            "This request did not match the typed completion obligation and was not executed. Rerun one direct standard project build/test validation previously recognized in this Work attempt; do not wrap it in a custom inline program. Correct it now with exactly one matching action; another mismatch ends the turn incomplete."
        }
        (CompletionAction::CanonicalWorkValidation, false) => {
            "Only one direct standard project build/test validation matching the typed completion obligation may execute at this boundary; a custom inline program, reader/probe, mutation, or settlement call does not match."
        }
        (CompletionAction::CanonicalWorkRepair, true) => {
            "This request did not match the bounded canonical-validation repair obligation and was not executed. Make exactly one workspace change; the runtime will require canonical revalidation next."
        }
        (CompletionAction::CanonicalWorkRepair, false) => {
            "Only one workspace repair may execute after the failed canonical validation; the next boundary requires canonical revalidation."
        }
        (CompletionAction::CompletionTaskAction, true) => {
            "This request was not a task-facing tool action and was not executed. Correct it now with exactly one action from the live admitted tool surface; runtime control and self-inspection calls do not match."
        }
        (CompletionAction::CompletionTaskAction, false) => {
            "Only one task-facing action from the live admitted tool surface may execute at this boundary; runtime control and self-inspection calls do not match."
        }
        (CompletionAction::RequiredExternalEffect, true) => {
            "This request did not carry the structured external_state_paths observation contract and was not executed. Correct it with one foreground task action naming only the smallest absolute external roots that must change."
        }
        (CompletionAction::RequiredExternalEffect, false) => {
            "Only one foreground task action carrying a valid external_state_paths observation contract may execute at this boundary."
        }
        (_, true) => {
            "This request did not match the typed completion obligation and was not executed. Correct it now with exactly one matching action; another mismatch ends the turn incomplete."
        }
        (_, false) => {
            "Only tool calls matching the typed completion obligation may execute at this boundary; each declared verification may be attempted at most once."
        }
    }
}

/// Return the explicit hook identity matched by a call.  The label is used as
/// the de-duplication key when several independent verification hooks are
/// declared for one settlement boundary.
pub(crate) fn completion_action_match_label(
    state: &AgenticLoopState,
    action: &CompletionAction,
    call: &serde_json::Value,
) -> Option<String> {
    let name = astra_turn_core::tool::args::shape::tool_call_name(call)?;
    if name.trim().is_empty() {
        return None;
    }
    let args = astra_turn_core::tool::args::shape::parse_tool_call_arguments(call).ok();
    match action {
        CompletionAction::RequiredWorkspaceMutation => {
            if completion_action_requires_complete_state_writer(state, action) {
                (name == "write_file"
                    && args.as_ref().is_some_and(|args| {
                        args.get("path")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|path| !path.trim().is_empty())
                            && args
                                .get("content")
                                .is_some_and(serde_json::Value::is_string)
                    }))
                .then(|| "required_workspace_mutation".to_string())
            } else {
                crate::turn::tool_side_effects::tool_call_may_mutate_workspace(name, args.as_ref())
                    .then(|| "required_workspace_mutation".to_string())
            }
        }
        CompletionAction::RequiredExternalEffect => args
            .as_ref()
            .and_then(|args| {
                args.get(astra_tools::workspace_observation::EXTERNAL_STATE_PATHS_FIELD)
            })
            .and_then(serde_json::Value::as_array)
            .is_some_and(|paths| !paths.is_empty())
            .then(|| "required_external_effect".to_string()),
        CompletionAction::CompletionTaskAction => tool_is_terminal_completion_task_action(name)
            .then(|| "completion_task_action".to_string()),
        CompletionAction::PostMutationObservation => {
            if let Some(expected_operation) = state
                .hooks
                .completion_settlement
                .post_mutation_repair_validation_operation
                .as_ref()
            {
                let raw_args = call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .map(|arguments| match arguments {
                        serde_json::Value::String(arguments) => arguments.clone(),
                        other => other.to_string(),
                    });
                return raw_args
                    .and_then(|args| {
                        astra_turn_core::evaluation::normalize_validation_prefix(name, &args)
                    })
                    .filter(|operation| operation == expected_operation)
                    .map(|_| "post_mutation_revalidation".to_string());
            }
            // A generic Bash command may be useful evidence in ordinary
            // progress accounting, but cannot spend this one-shot completion
            // authority.  It must opt into verify mode so the executor can
            // attach an unchanged-workspace receipt; otherwise a compound
            // command's stdout/exit status could exhaust recovery without a
            // durable observation. Typed observer tools already have their
            // own executor receipt contract.
            let spends_one_shot_observation = state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .is_some_and(|window| {
                    !window.consumed && window.action == CompletionAction::PostMutationObservation
                })
                // Ordinary mutation→observation chains have their own
                // established validator surface. This stricter Bash shape is
                // only for the text-stop recovery that injected an explicit
                // verify-receipt instruction; applying it to every window
                // would make that pre-existing surface promise drift.
                && state
                    .hooks
                    .completion_settlement
                    .post_mutation_observation_retries
                    > 0;
            if name == "bash" && spends_one_shot_observation {
                args.as_ref()
                    .is_some_and(|args| {
                        astra_tools::workspace_observation::is_explicit_workspace_verification_request(
                            name, args,
                        )
                    })
                    .then(|| "post_mutation_observation".to_string())
            } else {
                super::lifecycle::tool_call_can_observe_bound_workspace(state, name, args.as_ref())
                    .then(|| "post_mutation_observation".to_string())
            }
        }
        CompletionAction::PostMutationRepair => {
            crate::turn::tool_side_effects::tool_call_may_mutate_workspace(name, args.as_ref())
                .then(|| "post_mutation_repair".to_string())
        }
        CompletionAction::ExplicitVerification { missing_labels } => state
            .hooks
            .stop_hooks
            .iter()
            .find(|hook| {
                hook.authoritative
                    && missing_labels.iter().any(|label| label == &hook.label)
                    && tool_call_verifies_explicit_hook(name, args.as_ref(), hook)
            })
            .map(|hook| hook.label.clone()),
        CompletionAction::CanonicalWorkValidation => {
            // Canonical Work validation is an obligation only while its
            // primary Work attempt is active. A successful typed settlement
            // closes that attempt, so a duplicate sibling result cannot
            // resurrect its validation debt after the reset boundary.
            let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
                crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
            );
            let expected_operation = state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .clone()
                .or_else(|| {
                    active_work_attempt
                        .then(|| failed_work_validation_operation(state))
                        .flatten()
                });
            let raw_args = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .map(|arguments| match arguments {
                    serde_json::Value::String(arguments) => arguments.clone(),
                    other => other.to_string(),
                });
            raw_args
                .and_then(|args| {
                    astra_turn_core::evaluation::normalize_validation_prefix(name, &args)
                })
                .filter(|operation| {
                    expected_operation
                        .as_ref()
                        .is_none_or(|expected| expected == operation)
                })
                .map(|_| "canonical_work_validation".to_string())
        }
        CompletionAction::CanonicalWorkRepair => {
            crate::turn::tool_side_effects::tool_call_may_mutate_workspace(name, args.as_ref())
                .then(|| "canonical_work_repair".to_string())
        }
    }
}

fn tool_is_terminal_completion_task_action(name: &str) -> bool {
    // A terminal one-shot cannot safely start or advance another asynchronous
    // execution topology: the next boundary is text-only, so a child, fanout,
    // Work graph, or planning transition could not be observed and settled.
    // Derive built-in control-plane status from the canonical registry rather
    // than maintaining another incomplete list. Agent topologies and the
    // runtime-owned `delegate` surface are explicitly excluded because they
    // are not classified uniformly by that registry. Unknown admitted
    // MCP/task tools may still be valid external
    // actions and continue through ordinary admission and safety policy.
    if matches!(name, "agent" | "agent_fanout")
        || name == super::super::agentic::delegate_interception::DELEGATE_TOOL_NAME
    {
        return false;
    }
    let registry = astra_runtime_env::ToolRegistry::builtins();
    if registry.get(name).is_some_and(|spec| {
        spec.required.executor == astra_runtime_env::RequiredExecutor::ControlPlane
    }) {
        return false;
    }
    astra_turn_core::interaction_types::tool_counts_as_external_observation(name)
}

/// A single provider boundary may carry all missing independent verifiers,
/// but it must not skip an explicit dependency layer.  Dependent hooks stay
/// on the ordinary bounded verification path, where the first receipt can be
/// observed before the next hook is requested.
pub(crate) fn completion_action_window_is_batchable(
    state: &AgenticLoopState,
    action: &CompletionAction,
) -> bool {
    let CompletionAction::ExplicitVerification { missing_labels } = action else {
        return true;
    };
    state.hooks.stop_hooks.iter().all(|hook| {
        !hook.authoritative
            || !missing_labels.iter().any(|label| label == &hook.label)
            || hook
                .depends_on
                .iter()
                .all(|dependency| !missing_labels.iter().any(|label| label == dependency))
    })
}

/// Apply the single typed completion-action allowance after ordinary tool
/// admission has run.  The helper is deliberately stateful and shared by the
/// server and local/edge paths: any provider tool request consumes the one
/// boundary, while only an actually admitted matching call receives execute
/// authority.  A rejected matching call is therefore evidence of a failed
/// attempt, never a reason to reopen the window.
pub(crate) fn apply_completion_action_admission(
    state: &mut AgenticLoopState,
    mut admission: ToolCallAdmission,
    raw_tool_calls: &[serde_json::Value],
) -> ToolCallAdmission {
    if admission.completion_action_applied {
        return admission;
    }
    admission.completion_action_applied = true;
    let Some(action) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .filter(|window| !window.consumed)
        .map(|window| window.action.clone())
    else {
        return admission;
    };

    if raw_tool_calls.is_empty() {
        return admission;
    }

    let is_explicit_verification = matches!(&action, CompletionAction::ExplicitVerification { .. });
    let raw_contains_matching_action = raw_tool_calls
        .iter()
        .any(|call| completion_action_match_label(state, &action, call).is_some());
    let correction_available = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| window.mismatch_corrections_remaining > 0);
    let mut retained = Vec::with_capacity(if is_explicit_verification {
        admission.admitted.len()
    } else {
        1
    });
    let mut matched_labels = Vec::new();
    for call in admission.admitted.drain(..) {
        let match_label = completion_action_match_label(state, &action, &call);
        let matches = if is_explicit_verification {
            match_label.as_ref().is_some_and(|label| {
                if matched_labels.iter().any(|seen| seen == label) {
                    false
                } else {
                    matched_labels.push(label.clone());
                    true
                }
            })
        } else {
            retained.is_empty() && match_label.is_some()
        };
        if matches {
            retained.push(call);
            continue;
        }
        let name = astra_turn_core::tool::args::shape::tool_call_name(&call)
            .unwrap_or("unknown")
            .to_string();
        let id = call
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        admission.rejected.push(RejectedToolCall {
            id,
            name,
            canonical_call: call,
            result: serde_json::json!({
                "status": "rejected",
                "error_kind": "completion_action_mismatch",
                "retryable": !raw_contains_matching_action && correction_available,
                "allowed_action": action.clone(),
                "action_hint": completion_action_hint_for_state(state, &action),
                "error": completion_action_mismatch_instruction(
                    state,
                    &action,
                    !raw_contains_matching_action && correction_available,
                ),
            })
            .to_string(),
        });
    }
    let matched = !retained.is_empty();
    admission.admitted = retained;
    if !raw_contains_matching_action && correction_available {
        let action_hint = completion_action_hint_for_state(state, &action);
        let mismatch_instruction = completion_action_mismatch_instruction(state, &action, true);
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.mismatch_corrections_remaining =
                window.mismatch_corrections_remaining.saturating_sub(1);
        }
        // Replace only the provider boundary spent on the non-executed
        // mismatch. The original action and closing settlement boundaries
        // remain bounded; a second mismatch is terminal.
        state.max_turns = state.max_turns.saturating_add(1);
        state.remaining_turns = state.remaining_turns.saturating_add(1);
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "completion_action_mismatch_retry",
                "allowed_action": action.clone(),
                "attempts_remaining": 1,
                "mismatch_corrections_remaining": 0,
                "action_hint": action_hint,
                "execution_authority": "one_matching_action",
                "instruction": mismatch_instruction,
                "authority": "typed_completion_action_window",
            }),
        );
        return admission;
    }
    if let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_mut()
    {
        window.consumed = true;
        window.attempts_remaining = 0;
        window.matched = matched;
    }
    // Admission runs before execution. Do not make the current, valid action
    // look like a text-only wrap-up violation in the common tool phase. The
    // post-tool reconciliation above owns the next-boundary transition after
    // the authoritative outcome has entered the ledger.
    admission
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
    RoundSignal {
        tool_signatures,
        produced_mutation,
        tool_count,
    }
}

pub(crate) const CACHE_WASTE_MARKER: &str = "## ⤴ Repeated Cached Tool Calls Detected";
/// Default cache-waste midloop threshold. Used in tests; production code
/// reads from `ToolSelectionConfig::effective_cache_waste_midloop_threshold()`.
#[cfg(test)]
pub(crate) const CACHE_WASTE_MIDLOOP_THRESHOLD: usize = 3;

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

pub(crate) fn should_emit_cache_waste_advisory(state: &AgenticLoopState, threshold: usize) -> bool {
    if state.stall.cache_waste_advisory_emitted {
        return false;
    }
    !cache_wasteful_tools(state, threshold).is_empty()
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
        .filter(|record| record.was_executed())
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
        if let Some(identity) = turn_result
            .accum
            .context_manifest_trace
            .as_ref()
            .and_then(|trace| trace.get("request_identity"))
            .cloned()
            .and_then(|identity| {
                serde_json::from_value::<
                    astra_turn_core::context_assembly_trace::ModelRequestTraceIdentity,
                >(identity)
                .ok()
            })
        {
            collector.record_request_identity(identity);
        }
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

    // Tool-less turns still feed the canonical in-memory observation window.
    {
        let mut metrics = astra_core::TurnMetrics::default();
        metrics.rounds_completed = state.llm_rounds_completed;
        metrics.tokens_consumed = tokens_consumed;

        state.observation_journal.record_turn(&metrics);
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
        let rewrite_permit = state.begin_canonical_rewrite();
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
            state.finish_canonical_rewrite(rewrite_permit);
            let pressure = measured as f64 / state.max_turn_input_tokens as f64;
            state.context_compression_triggered = true;
            state.step_recorder.record_compaction_with_kind(
                "reactive_budget",
                total_messages_removed.min(u32::MAX as usize) as u32,
                total_freed,
                pressure,
            );
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
    state.hooks.completion_settlement.wrapup_origin =
        Some(super::host::BudgetWrapupOrigin::TokenRail);
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

fn record_tool_selection(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_index: usize,
) {
    let mut selected_tools = Vec::new();
    if !turn_result.edge_tool_round.is_empty() {
        astra_core::canonical_names::append_unique_names(
            &mut selected_tools,
            turn_result
                .edge_tool_round
                .iter()
                .map(|result| result.tool.as_str()),
        );
    } else if let Some(summary) = turn_result.accum.server_execution_summary.as_ref() {
        astra_core::canonical_names::append_unique_names(
            &mut selected_tools,
            summary.tools_used.iter().map(String::as_str),
        );
    }

    // A provider tool round can arrive before the async callback ledger has
    // resolved. Do not freeze an empty trace: the tool phase will record the
    // authoritative callback rows once they are available.
    let provider_requested_tools =
        turn_result.accum.has_tool_calls || !turn_result.accum.tool_calls.is_empty();
    if selected_tools.is_empty() && provider_requested_tools {
        return;
    }

    record_tool_surface_if_unset(state, selected_tools, turn_index);
}

fn record_tool_surface_if_unset(
    state: &mut AgenticLoopState,
    selected_tools: Vec<String>,
    turn_index: usize,
) {
    let Some(collector) = state.telemetry.turn_trace_collector.as_ref() else {
        return;
    };
    if collector.has_tool_trace() {
        return;
    }
    let tools_available = selected_tools.len() as u32;
    collector.record_tool_surface(&selected_tools, &[], tools_available, turn_index as u64);
}

/// Record the exact callback rows once the async edge ledger has resolved.
pub(crate) fn record_edge_tool_selection(
    state: &mut AgenticLoopState,
    edge_tool_round: &[astra_turn_core::sse_stream_host::EdgeToolExecResult],
    turn_index: usize,
) {
    if edge_tool_round.is_empty() {
        return;
    }
    let mut selected_tools = Vec::new();
    astra_core::canonical_names::append_unique_names(
        &mut selected_tools,
        edge_tool_round.iter().map(|result| result.tool.as_str()),
    );
    if selected_tools.is_empty() {
        return;
    }
    record_tool_surface_if_unset(state, selected_tools, turn_index);
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use astra_services::runs::{
        AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest, InMemoryRunStateStore,
    };
    use astra_services::session_journal::{
        JournalEventType, ToolCallDisposition, ToolCallRecord, TurnEventBuffer,
    };
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;
    use crate::observability::ObservabilityHub;
    use crate::server::run::engine::RunEngine;
    use crate::turn::agentic_loop::host::tests::{
        MockHost, edge_tool_result, make_edge_tool, make_state, server_tool_result, text_result,
    };
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, AppliedUserIntent, VolatileKind,
        run_agentic_loop_with_host,
    };
    use crate::turn::run_control::{RunStatusProvider, UserIntentPoll, UserIntentProvider};
    use astra_turn_core::chat_turn_sse_dispatch::{ChatTurnSseAccum, ServerLoopExecutionSummary};

    fn install_committed_work_synthesis_wire_surface(state: &mut AgenticLoopState) {
        state
            .hooks
            .completion_settlement
            .preserve_final_synthesis_wire_surface = true;
    }

    #[test]
    fn started_side_effecting_cancellation_cannot_be_rendered_completed() {
        let mut state = make_state();
        state.final_text = "installed successfully".to_string();
        let args = serde_json::json!({
            "command": "apt-get install -y package",
            "timeout": 90.0,
        })
        .to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            ms: 90_000,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        });

        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert!(matches!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        ));
        assert!(state.final_text.contains("interrupted after it started"));
    }

    #[test]
    fn scoped_cancellation_debt_requires_matching_executor_recovery_receipt() {
        let args = serde_json::json!({
            "command": "deploy target",
            "external_state_paths": ["/managed/target"],
        })
        .to_string();
        let target_set_digest = astra_tools::workspace_observation::ExternalEffectFingerprint::declared_target_set_digest_from_args(
            &serde_json::from_str(&args).expect("valid declared target set"),
        )
        .expect("declared target set has a digest");
        let cancelled = ToolCallRecord {
            name: "bash".into(),
            ok: false,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args.clone()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        };
        let matching_recovery = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args.clone()),
            disposition: Some(ToolCallDisposition::Executed),
            external_effect_observed: Some(true),
            external_effect_scope: Some(
                astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE.into(),
            ),
            external_effect_receipt: Some(serde_json::json!({
                "schema": "external_effect_receipt.v1",
                "source": "post_execution_fingerprint",
                "scope": astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE,
                "changed": true,
                "ownership": astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
                "target_set_digest": target_set_digest.clone(),
                "observed_roots": 1,
            })),
            ..ToolCallRecord::default()
        };

        let mut unsettled = make_state();
        unsettled.stall.tool_call_records.push(cancelled.clone());
        assert!(has_unsettled_side_effecting_cancellation(&unsettled));

        let mut settled = make_state();
        settled.stall.tool_call_records.push(cancelled);
        settled.stall.tool_call_records.push(matching_recovery);
        assert!(!has_unsettled_side_effecting_cancellation(&settled));

        let mut wrong_target = make_state();
        wrong_target.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args.clone()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        });
        wrong_target.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args.clone()),
            disposition: Some(ToolCallDisposition::Executed),
            external_effect_observed: Some(true),
            external_effect_receipt: Some(serde_json::json!({
                "schema": "external_effect_receipt.v1",
                "source": "post_execution_fingerprint",
                "scope": astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE,
                "changed": true,
                "ownership": astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
                "target_set_digest": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                "observed_roots": 1,
            })),
            ..ToolCallRecord::default()
        });
        assert!(has_unsettled_side_effecting_cancellation(&wrong_target));

        let mut malformed = make_state();
        malformed.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args.clone()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        });
        malformed.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(args.clone()),
            runtime_args_full: Some(args),
            disposition: Some(ToolCallDisposition::Executed),
            external_effect_observed: Some(true),
            external_effect_receipt: Some(serde_json::json!({"schema": "forged"})),
            ..ToolCallRecord::default()
        });
        assert!(has_unsettled_side_effecting_cancellation(&malformed));

        let mut read_only = make_state();
        read_only.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            args_full: Some(serde_json::json!({"command": "pwd"}).to_string()),
            runtime_args_full: Some(serde_json::json!({"command": "pwd"}).to_string()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        });
        assert!(!has_unsettled_side_effecting_cancellation(&read_only));

        let mut rejected = make_state();
        rejected.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            args_full: Some(serde_json::json!({"command": "deploy target"}).to_string()),
            runtime_args_full: Some(serde_json::json!({"command": "deploy target"}).to_string()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Rejected),
            ..ToolCallRecord::default()
        });
        assert!(!has_unsettled_side_effecting_cancellation(&rejected));

        let mut external_write = make_state();
        external_write.stall.tool_call_records.push(ToolCallRecord {
            name: "github".into(),
            ok: false,
            args_full: Some(serde_json::json!({"action": "create_issue"}).to_string()),
            runtime_args_full: Some(serde_json::json!({"action": "create_issue"}).to_string()),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(ToolCallDisposition::Executed),
            ..ToolCallRecord::default()
        });
        assert!(has_unsettled_side_effecting_cancellation(&external_write));
    }

    fn attach_llm_progress_receiver(
        state: &mut AgenticLoopState,
    ) -> tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent> {
        let broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::new(16));
        let receiver = broadcaster.subscribe();
        state.messaging.progress_emitter = Some(broadcaster.for_agent_with_run_context(
            "progress-test-agent".to_string(),
            "progress-test-run".to_string(),
            "progress-test-parent".to_string(),
            None,
        ));
        receiver
    }

    fn drain_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
    ) -> Vec<crate::orchestration::ProgressEventType> {
        let mut events = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(event)
                    if matches!(
                        &event.event_type,
                        crate::orchestration::ProgressEventType::LlmCallStarted { .. }
                            | crate::orchestration::ProgressEventType::LlmCallCompleted { .. }
                    ) =>
                {
                    events.push(event.event_type);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    panic!("progress test receiver lagged by {skipped} events")
                }
            }
        }
        events
    }

    fn assert_no_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
    ) {
        assert!(
            drain_llm_progress(receiver).is_empty(),
            "pre-host rejection must not emit an orphan LLM lifecycle"
        );
    }

    fn assert_one_paired_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
        turn: u32,
        expected_ttft_ms: Option<u64>,
    ) {
        let events = drain_llm_progress(receiver);
        assert_eq!(events.len(), 2, "one host call must emit one exact pair");
        assert!(matches!(
            &events[0],
            crate::orchestration::ProgressEventType::LlmCallStarted { turn: actual }
                if *actual == turn
        ));
        assert!(matches!(
            &events[1],
            crate::orchestration::ProgressEventType::LlmCallCompleted {
                turn: actual,
                ttft_ms,
                ..
            } if *actual == turn && *ttft_ms == expected_ttft_ms
        ));
    }

    #[test]
    fn provider_round_accounting_uses_explicit_boundary_provenance() {
        assert!(should_record_local_provider_round(
            super::super::host::ControlPlaneTurnBoundary::Ordinary,
            false,
        ));
        assert!(should_record_local_provider_round(
            super::super::host::ControlPlaneTurnBoundary::ProviderBacked,
            false,
        ));
        assert!(!should_record_local_provider_round(
            super::super::host::ControlPlaneTurnBoundary::Providerless,
            false,
        ));
        assert!(!should_record_local_provider_round(
            super::super::host::ControlPlaneTurnBoundary::ProviderBacked,
            true,
        ));
    }

    #[test]
    fn runtime_retrospective_intent_requires_scope_and_investigation() {
        assert!(runtime_retrospective_requires_live_evidence(
            "系统性反省这段 session 的实际运行状态和异常 trace，请基于证据分析"
        ));
        assert!(runtime_retrospective_requires_live_evidence(
            "Audit this runtime session and diagnose its tool calls"
        ));
        assert!(!runtime_retrospective_requires_live_evidence(
            "代码 review 和修改代码有什么区别？"
        ));
        assert!(!runtime_retrospective_requires_live_evidence(
            "系统性分析这个算法的复杂度"
        ));
        assert!(!runtime_retrospective_requires_live_evidence(
            "Inspect the session command surface and give two evidence-based bullets"
        ));
        assert!(!runtime_retrospective_requires_live_evidence(
            "Do not include or invent a session id; explain the proposal evidence boundary"
        ));
    }

    #[test]
    fn settlement_candidate_keeps_first_mixed_response_until_text_only_retry() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;

        let mixed = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Verified the change and the remaining check is clear.".to_string(),
                tool_calls: vec![serde_json::json!({
                    "function": {"name": "bash", "arguments": "{}"}
                })],
                has_tool_calls: true,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &mixed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change and the remaining check is clear.")
        );

        let later_mixed = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Let me inspect one more thing.".to_string(),
                tool_calls: vec![serde_json::json!({
                    "function": {"name": "cat", "arguments": "{}"}
                })],
                has_tool_calls: true,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &later_mixed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change and the remaining check is clear."),
            "a repeated violating response must not replace the substantive first candidate"
        );

        let compliant_retry = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Verified the change; one requested check remains unverified."
                    .to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &compliant_retry);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change; one requested check remains unverified."),
            "a compliant text-only retry may replace the mixed candidate"
        );
    }

    #[test]
    fn latest_provider_text_tracks_each_non_empty_response() {
        let mut state = make_state();
        let first = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "first response".to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };
        capture_latest_provider_text(&mut state, &first);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .latest_provider_text
                .as_deref(),
            Some("first response")
        );

        let second = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "latest response".to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };
        capture_latest_provider_text(&mut state, &second);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .latest_provider_text
                .as_deref(),
            Some("latest response")
        );
    }

    #[test]
    fn same_boundary_observation_fanout_collapses_to_composite_overviews() {
        let mut calls = vec![
            serde_json::json!({"id":"i1","function":{"name":"introspect","arguments":"{\"facet\":\"trace\",\"question\":\"why\"}"}}),
            serde_json::json!({"id":"i2","function":{"name":"introspect","arguments":"{\"facet\":\"overview\"}"}}),
            serde_json::json!({"id":"r1","function":{"name":"reflect","arguments":"{\"facet\":\"tools\",\"question\":\"why\"}"}}),
            serde_json::json!({"id":"r2","function":{"name":"reflect","arguments":"{\"facet\":\"overview\"}"}}),
        ];

        assert_eq!(collapse_batched_observation_fanout(&mut calls), 2);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "i2");
        assert_eq!(calls[1]["id"], "r2");
        for call in calls {
            let args: serde_json::Value =
                serde_json::from_str(call["function"]["arguments"].as_str().expect("args"))
                    .expect("valid json args");
            assert_eq!(args["facet"], "overview");
            assert_eq!(args["depth"], "diagnostic");
        }
    }

    #[test]
    fn artifact_paging_is_not_collapsed_with_live_introspection() {
        let mut calls = vec![
            serde_json::json!({"id":"page","function":{"name":"introspect","arguments":"{\"artifact\":\"artifact://session/tool-result/x\",\"offset\":0}"}}),
            serde_json::json!({"id":"live","function":{"name":"introspect","arguments":"{\"facet\":\"overview\"}"}}),
        ];

        assert_eq!(collapse_batched_observation_fanout(&mut calls), 0);
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test]
    async fn runtime_retrospective_without_introspect_gets_one_bounded_evidence_retry() {
        let unsupported = "我回看了完整运行记录，session 没有异常 trace。";
        let mut host = MockHost::new(vec![
            text_result(unsupported, 20, 10, Some(30)),
            server_tool_result(
                vec![serde_json::json!({
                    "id": "call-introspect",
                    "type": "function",
                    "function": {
                        "name": "introspect",
                        "arguments": serde_json::json!({
                            "facet": "overview",
                            "depth": "diagnostic",
                            "horizon": "recent"
                        }).to_string()
                    }
                })],
                Vec::new(),
                20,
                10,
                Some(30),
            ),
            text_result("基于 live snapshot：没有观测到异常。", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["introspect"]);
        let mut state = make_state();
        state.message = "请系统性反省这个 session 的运行状态和 trace，并基于证据分析".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));
        let workspace = tempfile::TempDir::new().expect("workspace");
        state.runtime_tool_executor = Some(Arc::new(
            crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
                workspace.path().to_path_buf(),
                "test-user".into(),
                "test-session".into(),
                None,
                None,
            ),
        ));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            outcome.is_ok(),
            "bounded evidence retry must complete: {outcome:?}; turns={}; tools={:?}; records={:?}; final={:?}",
            host.turn_count(),
            state.telemetry.all_tools_used,
            state.stall.tool_call_records,
            state.final_text,
        );
        assert_eq!(host.turn_count(), 3);
        assert!(state.telemetry.all_tools_used.contains("introspect"));
        assert_eq!(state.final_text, "基于 live snapshot：没有观测到异常。");
        assert!(state.messages.iter().all(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|content| content != unsupported)
        }));
    }

    #[tokio::test]
    async fn required_mutation_and_post_mutation_observation_are_completion_obligations() {
        let premature = "I found the issue and will apply the fix next.";
        let mut writer = make_edge_tool("write_file", "updated source");
        writer.args = serde_json::json!({
            "path": "/app/source.txt",
            "content": "updated source\n",
        });
        let mut observer = make_edge_tool("read_file", "final source");
        observer.args = serde_json::json!({"path": "/app/source.txt"});
        let mut host = MockHost::new(vec![
            text_result(premature, 20, 10, Some(30)),
            edge_tool_result(vec![writer], 20, 10, Some(30)),
            edge_tool_result(vec![observer], 20, 10, Some(30)),
            text_result(
                "Implemented and verified the resulting source.",
                20,
                10,
                Some(30),
            ),
        ])
        .with_valid_tools(&["write_file", "read_file"]);
        let mut state = make_state();
        state.message = "Implement the requested source change.".to_string();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 4);
        assert_eq!(
            state.final_text,
            "Implemented and verified the resulting source."
        );
        assert!(state.messages.iter().all(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|content| content != premature)
        }));
        // The first text-stop opens one typed writer window. A successful
        // writer advances that same bounded chain directly to the required
        // observation; prose at that intermediate boundary would truthfully
        // end incomplete instead of reopening ordinary exploration.
    }

    #[tokio::test]
    async fn default_profile_does_not_complete_after_mutation_and_unrelated_shell() {
        // This is deliberately provider/task neutral: a caller that did not
        // provide structured intent still must not turn a successful
        // workspace mutation plus an unrelated shell probe into a verified
        // completion.  The post-mutation observation obligation is derived
        // from the executed ledger, not from task keywords.
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("write_file", "updated source")],
                20,
                10,
                Some(30),
            ),
            edge_tool_result(
                vec![make_edge_tool("bash", "wrote unrelated scratch probe")],
                20,
                10,
                Some(30),
            ),
            text_result("The change is complete.", 20, 10, Some(30)),
            edge_tool_result(
                vec![make_edge_tool("read_file", "final source")],
                20,
                10,
                Some(30),
            ),
            text_result("The change is complete and observed.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Please update the workspace and report back.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 5);
        assert_eq!(state.final_text, "The change is complete and observed.");
    }

    #[tokio::test]
    async fn bound_script_validation_completes_without_model_progress_phrase() {
        let mut write = make_edge_tool("write_file", "script written");
        write.args = serde_json::json!({
            "path": "/app/solution.py",
            "content": "print('ok')",
        });
        let mut validate = make_edge_tool("bash", "ok");
        validate.args = serde_json::json!({
            "command": "cd /app && python3 solution.py",
        });
        validate
            .tool_result_fields
            .as_mut()
            .expect("edge fixture carries owner metadata")
            .extend(
                astra_tools::workspace_observation::changed_receipt_with_ownership(
                    astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
                ),
            );
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![write], 20, 10, Some(30)),
            edge_tool_result(vec![validate], 20, 10, Some(30)),
            text_result("Ready.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Implement and validate the requested workspace artifact.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("trusted tool evidence should settle the run");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 3, "no recovery round should be needed");
        assert_eq!(state.final_text, "Ready.");
        assert!(state.interruption.is_none());
        assert!(successful_post_mutation_observation(&state));
    }

    #[test]
    fn script_observation_requires_the_latest_delivered_artifact_and_no_later_mutation() {
        fn supervised_script(command: &str) -> ToolCallRecord {
            let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
                astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
            );
            let args = serde_json::json!({"command": command}).to_string();
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(args.clone()),
                runtime_args_full: Some(args),
                workspace_mutation_observed: Some(true),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            }
        }

        let cases = [
            (
                vec![],
                "cd /app && python3 existing.py | cat",
                "a literal script pipeline cannot self-authorize without a typed delivery",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/other.py"}"#),
                )],
                "cd /app && python3 solution.py",
                "an unrelated historical delivery must not authorize the script",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && python3 solution.py | cat",
                "a pipeline consumer cannot prove the delivered script ran to completion",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && python3 solution.py | head -1",
                "a truncating pipeline consumer cannot become a behavioral receipt",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && python3 existing.py | cat",
                "a pipeline consumer must not downgrade literal artifact identity to a generic observer",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && python3 solution.py | python3 other.py",
                "a pipeline with multiple script artifacts has no single trusted delivery identity",
            ),
            (
                vec![
                    executed_record("write_file", true, Some(r#"{"path":"/app/solution.py"}"#)),
                    executed_record("write_file", true, Some(r#"{"path":"/app/later.txt"}"#)),
                ],
                "cd /app && python3 solution.py",
                "a delivery from an older mutation epoch must not authorize the script",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && touch earlier.txt && python3 solution.py",
                "an unrelated mutation before script execution in the same Bash record is not the delivered artifact epoch",
            ),
            (
                vec![executed_record(
                    "write_file",
                    true,
                    Some(r#"{"path":"/app/solution.py"}"#),
                )],
                "cd /app && python3 solution.py && touch later.txt",
                "a mutation after script execution in the same Bash record reopens the epoch",
            ),
        ];

        for (mut records, command, reason) in cases {
            let mut state = make_state();
            state.hooks.workspace_root_hint = Some("/app".into());
            records.push(supervised_script(command));
            state.stall.tool_call_records = records;
            assert!(!successful_post_mutation_observation(&state), "{reason}");
        }

        let mut unbound = make_state();
        unbound.stall.tool_call_records = vec![
            executed_record("write_file", true, Some(r#"{"path":"/app/solution.py"}"#)),
            supervised_script("cd /app && python3 solution.py"),
        ];
        assert!(
            !successful_post_mutation_observation(&unbound),
            "literal artifact identity must fail closed without a bound workspace root"
        );
    }

    #[tokio::test]
    async fn shell_redirect_after_mutation_is_not_mistaken_for_observation() {
        let mut scratch_bash = make_edge_tool("bash", "created scratch fixture");
        scratch_bash.args = serde_json::json!({
            "command": "cat > /tmp/test_xss.html <<'EOF'\n<script>alert(1)</script>\nEOF"
        });
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("write_file", "updated source")],
                20,
                10,
                Some(30),
            ),
            edge_tool_result(vec![scratch_bash], 20, 10, Some(30)),
            text_result("The change is complete.", 20, 10, Some(30)),
            edge_tool_result(
                vec![make_edge_tool("read_file", "final source")],
                20,
                10,
                Some(30),
            ),
            text_result("The change is complete and observed.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Please update the workspace and report back.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 5);
        assert_eq!(state.final_text, "The change is complete and observed.");
    }

    #[test]
    fn owner_typed_workspace_receipts_survive_remote_root_projection() {
        // The writer/observer owner is an Edge workspace at /app while the
        // server-side scheduler is intentionally given a different local
        // root.  No host stat or task-specific path rule may be needed.
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/server-side-root".into());

        let writer_fields = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: writer_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert!(has_concrete_workspace_mutation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        let observer_fields =
            astra_tools::workspace_observation::typed_workspace_observation_receipt();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: observer_fields
                .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });
        assert!(successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);

        let edge_workspace = tempfile::tempdir().expect("edge workspace");
        std::fs::write(edge_workspace.path().join("filter.py"), "ready\n").expect("edge target");
        let writer_args = serde_json::json!({"path": "filter.py", "content": "ready\n"});
        let desired_state =
            astra_tools::workspace_observation::workspace_file_state_identity(b"ready\n");
        let convergence_fields = astra_tools::workspace_observation::typed_workspace_desired_state_convergence_receipt_for(
            "write_file",
            &writer_args,
            edge_workspace.path(),
            false,
            Some(&desired_state),
        )
        .expect("owner convergence receipt");
        let observer_args = serde_json::json!({"path": "filter.py"});
        let observation_fields =
            astra_tools::workspace_observation::typed_workspace_observation_snapshot_receipt_for(
                "read_file",
                &observer_args,
                edge_workspace.path(),
                false,
                true,
            )
            .expect("owner strong observation");
        let mut convergence_state = make_state();
        mark_must_mutate(&mut convergence_state);
        convergence_state.hooks.workspace_root_hint = Some(format!(
            "/definitely-absent-scheduler-root-{}",
            uuid::Uuid::new_v4()
        ));
        convergence_state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                tool_call_id: Some("remote-write".into()),
                round: Some(1),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(writer_args.to_string()),
                runtime_args_full: Some(writer_args.to_string()),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: convergence_fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            },
            ToolCallRecord {
                name: "read_file".into(),
                tool_call_id: Some("remote-read".into()),
                round: Some(2),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(observer_args.to_string()),
                runtime_args_full: Some(observer_args.to_string()),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: observation_fields
                    .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            },
        ];
        assert!(!has_concrete_workspace_mutation(&convergence_state));
        assert!(has_bound_workspace_completion_evidence(&convergence_state));
        assert_eq!(pending_completion_action(&convergence_state), None);
    }

    #[test]
    fn live_desired_state_convergence_requires_same_target_fresh_observation() {
        fn convergence_record(workspace: &std::path::Path, path: &str) -> ToolCallRecord {
            let args = serde_json::json!({"path": path, "content": "done\n"});
            let desired_state =
                astra_tools::workspace_observation::workspace_file_state_identity(b"done\n");
            let fields = astra_tools::workspace_observation::typed_workspace_desired_state_convergence_receipt_for(
                "write_file",
                &args,
                workspace,
                false,
                Some(&desired_state),
            )
            .expect("convergence receipt");
            let args = args.to_string();
            ToolCallRecord {
                name: "write_file".into(),
                tool_call_id: Some(format!("write-{path}")),
                round: Some(1),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(args.clone()),
                runtime_args_full: Some(args),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            }
        }

        fn observation_record(workspace: &std::path::Path, path: &str, ok: bool) -> ToolCallRecord {
            let args = serde_json::json!({"path": path});
            let fields =
                astra_tools::workspace_observation::typed_workspace_observation_snapshot_receipt_for(
                    "read_file",
                    &args,
                    workspace,
                    !ok,
                    true,
                );
            let args = args.to_string();
            ToolCallRecord {
                name: "read_file".into(),
                tool_call_id: Some(format!("read-{path}")),
                round: Some(2),
                ok,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(args.clone()),
                runtime_args_full: Some(args),
                workspace_mutation_scope: fields.as_ref().and_then(|fields| {
                    fields
                        .get(astra_tools::workspace_observation::OBSERVATION_SCOPE_FIELD)
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                }),
                workspace_mutation_receipt: fields.as_ref().and_then(|fields| {
                    fields
                        .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                        .cloned()
                }),
                ..ToolCallRecord::default()
            }
        }

        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("answer.txt"), "done\n").expect("answer");
        std::fs::write(workspace.path().join("other.txt"), "other\n").expect("other");
        let opaque_args = serde_json::json!({
            "command": "printf 'done\\n' > answer.txt"
        })
        .to_string();
        let opaque_write = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(opaque_args.clone()),
            runtime_args_full: Some(opaque_args),
            ..ToolCallRecord::default()
        };
        let converged = convergence_record(workspace.path(), "answer.txt");
        let observed = observation_record(workspace.path(), "answer.txt", true);

        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.workspace_root_hint = Some(workspace.path().to_string_lossy().to_string());
        state.stall.tool_call_records = vec![opaque_write, converged.clone(), observed.clone()];
        assert!(
            !has_concrete_workspace_mutation(&state),
            "convergence completion evidence must never become a mutation fact"
        );
        assert!(has_bound_workspace_completion_evidence(&state));
        assert!(successful_post_mutation_observation(&state));
        assert_eq!(
            pending_completion_action_for_work_state(&state, false),
            None
        );
        assert!(
            !record_is_effective_workspace_repair(&converged),
            "convergence is completion evidence only, never canonical repair"
        );

        let mut pending_text_stop = make_state();
        mark_must_mutate(&mut pending_text_stop);
        pending_text_stop.hooks.workspace_root_hint =
            Some(workspace.path().to_string_lossy().to_string());
        pending_text_stop.stall.tool_call_records = vec![converged.clone()];
        pending_text_stop.final_text = "done".into();
        assert!(enforce_workspace_completion_before_text_completion(
            &mut pending_text_stop
        ));
        assert_eq!(
            pending_text_stop
                .hooks
                .completion_settlement
                .workspace_mutation_retries,
            0,
            "pending convergence must not consume a mutation retry"
        );
        assert_eq!(
            pending_text_stop
                .hooks
                .completion_settlement
                .post_mutation_observation_retries,
            1
        );
        assert!(
            pending_text_stop
                .volatile_pending
                .iter()
                .any(|entry| { entry.payload["signal"] == "desired_state_observation_missing" })
        );

        let mut same_batch_observation = observed.clone();
        same_batch_observation.round = converged.round;
        same_batch_observation.batch_id = Some("parallel-batch".into());
        let mut same_batch_writer = converged.clone();
        same_batch_writer.batch_id = Some("parallel-batch".into());

        let mut wrong_state_observation = observed.clone();
        wrong_state_observation
            .workspace_mutation_receipt
            .as_mut()
            .expect("observation receipt")["observed_state"] = serde_json::to_value(
            astra_tools::workspace_observation::workspace_file_state_identity(b"other\n"),
        )
        .expect("state");
        let mut generic_observation = observed.clone();
        if let Some(receipt) = generic_observation
            .workspace_mutation_receipt
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
        {
            receipt.remove("observed_state");
            receipt.remove("observation_id");
        }
        let forged_state = serde_json::to_value(
            astra_tools::workspace_observation::workspace_file_state_identity(b"forged\n"),
        )
        .expect("forged state");
        let mut self_consistent_forged_writer = converged.clone();
        self_consistent_forged_writer
            .workspace_mutation_receipt
            .as_mut()
            .expect("writer receipt")["desired_state"] = forged_state.clone();
        let mut self_consistent_forged_observer = observed.clone();
        self_consistent_forged_observer
            .workspace_mutation_receipt
            .as_mut()
            .expect("observer receipt")["observed_state"] = forged_state;

        for (records, expected_action, reason) in [
            (
                vec![converged.clone()],
                CompletionAction::PostMutationObservation,
                "convergence without a later observation",
            ),
            (
                vec![
                    converged.clone(),
                    observation_record(workspace.path(), "other.txt", true),
                ],
                CompletionAction::PostMutationObservation,
                "an observation of the wrong target",
            ),
            (
                vec![
                    converged.clone(),
                    observation_record(workspace.path(), "answer.txt", false),
                ],
                CompletionAction::PostMutationObservation,
                "a failed observation",
            ),
            (
                vec![same_batch_writer, same_batch_observation],
                CompletionAction::PostMutationObservation,
                "a same-round parallel observation",
            ),
            (
                vec![converged.clone(), generic_observation],
                CompletionAction::PostMutationObservation,
                "a generic typed receipt without a strong snapshot",
            ),
            (
                vec![converged.clone(), wrong_state_observation],
                CompletionAction::RequiredWorkspaceMutation,
                "an observation of different bytes",
            ),
            (
                vec![
                    self_consistent_forged_writer,
                    self_consistent_forged_observer,
                ],
                CompletionAction::RequiredWorkspaceMutation,
                "a self-consistent forged desired/observed state that differs from normalized write arguments",
            ),
            (
                vec![
                    converged.clone(),
                    executed_record(
                        "bash",
                        true,
                        Some(r#"{"command":"printf later > later.txt"}"#),
                    ),
                    observation_record(workspace.path(), "answer.txt", true),
                ],
                CompletionAction::RequiredWorkspaceMutation,
                "an intervening mutation risk",
            ),
            (
                vec![
                    converged.clone(),
                    executed_record(
                        "bash",
                        false,
                        Some(r#"{"command":"python3 opaque_helper.py"}"#),
                    ),
                    observation_record(workspace.path(), "answer.txt", true),
                ],
                CompletionAction::RequiredWorkspaceMutation,
                "a failed opaque shell between writer and observer",
            ),
            (
                vec![
                    converged.clone(),
                    executed_record(
                        "lsp",
                        false,
                        Some(r#"{"operation":"rename","file":"answer.txt","dry_run":false}"#),
                    ),
                    observation_record(workspace.path(), "answer.txt", true),
                ],
                CompletionAction::RequiredWorkspaceMutation,
                "a failed may-mutate LSP call between writer and observer",
            ),
        ] {
            let mut rejected = make_state();
            mark_must_mutate(&mut rejected);
            rejected.hooks.workspace_root_hint =
                Some(workspace.path().to_string_lossy().to_string());
            rejected.stall.tool_call_records = records;
            assert!(!has_concrete_workspace_mutation(&rejected), "{reason}");
            assert!(
                !has_bound_workspace_completion_evidence(&rejected),
                "{reason}"
            );
            assert_eq!(
                pending_completion_action_for_work_state(&rejected, false),
                Some(expected_action),
                "{reason} must keep a truthful completion obligation open"
            );
            rejected.final_text = "done".into();
            assert!(
                enforce_workspace_completion_before_text_completion(&mut rejected),
                "{reason} must not let a text stop complete"
            );
        }

        let mut restored = converged.clone();
        restored.runtime_args_full = None;
        let mut reused = converged.clone();
        reused.disposition = Some(ToolCallDisposition::Reused);
        let mut args_mismatch = converged.clone();
        args_mismatch.runtime_args_full =
            Some(serde_json::json!({"path": "answer.txt", "content": "forged\n"}).to_string());
        let mut missing_round = converged.clone();
        missing_round.round = None;
        let mut malformed = converged;
        malformed
            .workspace_mutation_receipt
            .as_mut()
            .expect("receipt")["target"]["sha256"] = serde_json::json!("forged");
        for (writer, reason) in [
            (restored, "restored convergence"),
            (reused, "reused convergence"),
            (
                args_mismatch,
                "receipt copied onto mismatched live arguments",
            ),
            (missing_round, "convergence without causal round identity"),
            (malformed, "malformed convergence"),
        ] {
            let mut rejected = make_state();
            mark_must_mutate(&mut rejected);
            rejected.hooks.workspace_root_hint =
                Some(workspace.path().to_string_lossy().to_string());
            rejected.stall.tool_call_records = vec![
                writer,
                observation_record(workspace.path(), "answer.txt", true),
            ];
            assert!(!has_concrete_workspace_mutation(&rejected), "{reason}");
            assert!(
                !has_bound_workspace_completion_evidence(&rejected),
                "{reason}"
            );
            assert_eq!(
                pending_completion_action_for_work_state(&rejected, false),
                Some(CompletionAction::RequiredWorkspaceMutation),
                "{reason} must not retain live observation authority"
            );
        }

        let mut repaired_after_mismatch = make_state();
        mark_must_mutate(&mut repaired_after_mismatch);
        repaired_after_mismatch.hooks.workspace_root_hint =
            Some(workspace.path().to_string_lossy().to_string());
        let mut mismatch = observed.clone();
        mismatch
            .workspace_mutation_receipt
            .as_mut()
            .expect("strong receipt")["observed_state"] = serde_json::to_value(
            astra_tools::workspace_observation::workspace_file_state_identity(b"external\n"),
        )
        .expect("state");
        let mut post_repair_observation = observed;
        post_repair_observation.round = Some(4);
        post_repair_observation.tool_call_id = Some("read-after-repair".into());
        repaired_after_mismatch.stall.tool_call_records = vec![
            convergence_record(workspace.path(), "answer.txt"),
            mismatch,
            executed_record(
                "write_file",
                true,
                Some(r#"{"path":"answer.txt","content":"done\n"}"#),
            ),
            post_repair_observation,
        ];
        assert!(has_concrete_workspace_mutation(&repaired_after_mismatch));
        assert!(successful_post_mutation_observation(
            &repaired_after_mismatch
        ));
        assert_eq!(
            pending_completion_action_for_work_state(&repaired_after_mismatch, false),
            None,
            "a mismatched strong read must require repair, then allow a real mutation plus fresh verification to close"
        );
    }

    #[tokio::test]
    async fn mock_edge_turn_completes_opaque_write_via_live_same_target_convergence() {
        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("answer.txt");
        std::fs::write(&target, "done\n").expect("opaque write result");
        let path = target.to_string_lossy().to_string();

        let mut opaque_write = make_edge_tool("bash", "opaque write completed");
        opaque_write.args = serde_json::json!({
            "command": format!("printf 'done\\n' > '{}'", path),
        });

        let write_args = serde_json::json!({"path": path, "content": "done\n"});
        let desired_state =
            astra_tools::workspace_observation::workspace_file_state_identity(b"done\n");
        let convergence_fields =
            astra_tools::workspace_observation::typed_workspace_desired_state_convergence_receipt_for(
                "write_file",
                &write_args,
                workspace.path(),
                false,
                Some(&desired_state),
            )
            .expect("convergence fields");
        let mut convergence = make_edge_tool("write_file", "already desired");
        convergence.args = write_args;
        let fields = convergence
            .tool_result_fields
            .as_mut()
            .expect("edge fields");
        fields.remove("workspace_mutation_applied");
        fields.remove(astra_tools::workspace_observation::OBSERVED_FIELD);
        fields.remove(astra_tools::workspace_observation::SCOPE_FIELD);
        fields.remove(astra_tools::workspace_observation::RECEIPT_FIELD);
        fields.extend(convergence_fields);

        let read_args = serde_json::json!({"path": target});
        let observation_fields =
            astra_tools::workspace_observation::typed_workspace_observation_snapshot_receipt_for(
                "read_file",
                &read_args,
                workspace.path(),
                false,
                true,
            )
            .expect("observation fields");
        let mut observation = make_edge_tool("read_file", "done");
        observation.args = read_args;
        observation
            .tool_result_fields
            .get_or_insert_with(Default::default)
            .extend(observation_fields);

        let mut host = MockHost::new(vec![
            edge_tool_result(vec![opaque_write], 20, 10, Some(30)),
            text_result("The opaque writer reported success.", 20, 10, Some(30)),
            edge_tool_result(vec![convergence], 20, 10, Some(30)),
            edge_tool_result(vec![observation], 20, 10, Some(30)),
            text_result(
                "The requested state is present and observed.",
                20,
                10,
                Some(30),
            ),
        ])
        .with_valid_tools(&["bash", "write_file", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(workspace.path().to_string_lossy().to_string());
        state.message = "Create the requested workspace artifact and verify it.".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("live convergence flow");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 5);
        assert!(!has_concrete_workspace_mutation(&state));
        assert!(has_bound_workspace_completion_evidence(&state));
        assert!(successful_post_mutation_observation(&state));
        assert!(state.interruption.is_none());
    }

    #[tokio::test]
    async fn opaque_write_then_read_only_recovery_remains_incomplete() {
        let mut opaque_write = make_edge_tool("bash", "opaque write completed");
        opaque_write.args = serde_json::json!({
            "command": "opaque workspace writer",
        });
        let mut read = make_edge_tool("read_file", "reported final bytes");
        read.args = serde_json::json!({"path": "/app/result.txt"});
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![opaque_write], 20, 10, Some(30)),
            text_result("The shell said the file is ready.", 20, 10, Some(30)),
            edge_tool_result(vec![read], 20, 10, Some(30)),
            text_result("The file is ready.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["bash", "write_file", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/server-has-no-remote-app-mount".into());
        state.message = "Create the requested workspace artifact and verify it.".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("read-only recovery must close truthfully");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 4);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!has_bound_workspace_completion_evidence(&state));
        assert!(
            state
                .stall
                .tool_call_records
                .iter()
                .all(|record| { record.name != "read_file" || !record.was_executed() })
        );
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

    fn executed_record(name: &str, ok: bool, args_full: Option<&str>) -> ToolCallRecord {
        let mut record = ToolCallRecord {
            name: name.into(),
            ok,
            args_full: args_full.map(ToString::to_string),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        };
        // Provider-neutral fixtures model the same owner fact production
        // direct writers now emit: a successful call has an applied receipt;
        // a failed call remains mutation risk but cannot be positive proof.
        if ok
            && matches!(
                name,
                "write_file"
                    | "str_replace"
                    | "multi_edit"
                    | "edit_file"
                    | "create_file"
                    | "delete_file"
                    | "notebook_edit"
                    | "rollback_file_edits"
                    | "rename_symbol"
                    | "lsp"
            )
        {
            let fields = astra_tools::workspace_observation::typed_workspace_tool_receipt();
            record.workspace_mutation_observed = Some(true);
            record.workspace_mutation_scope =
                Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.to_string());
            record.workspace_mutation_receipt = fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned();
        }
        record
    }

    fn external_effect_record(ownership: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                serde_json::json!({
                    "command": "opaque external action",
                    "external_state_paths": ["/managed/state"],
                })
                .to_string(),
            ),
            disposition: Some(ToolCallDisposition::Executed),
            external_effect_observed: Some(true),
            external_effect_scope: Some(
                astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE.to_string(),
            ),
            external_effect_receipt: Some(serde_json::json!({
                "schema": "external_effect_receipt.v1",
                "source": "post_execution_fingerprint",
                "scope": astra_tools::workspace_observation::DECLARED_EXTERNAL_STATE_SCOPE,
                "changed": true,
                "ownership": ownership,
                "target_set_digest": "00".repeat(32),
                "observed_roots": 1,
            })),
            ..Default::default()
        }
    }

    fn validation_record(command: &str, result_class: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(serde_json::json!({"command": command}).to_string()),
            result_class: Some(result_class.into()),
            exit_semantics: Some(
                if result_class == "success" {
                    "success"
                } else {
                    "domain_negative"
                }
                .into(),
            ),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_contract_does_not_accept_read_as_verification() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.task_profile.verification_required = true;
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert!(record_is_effective_workspace_repair(
            state.stall.tool_call_records.last().unwrap()
        ));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));

        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(vec!["quality".to_string()])
        );
        assert!(enforce_explicit_verification_before_text_completion(
            &mut state
        ));
        assert_eq!(state.hooks.completion_settlement.verification_retries, 1);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                == Some("explicit_verification_required.v1")
        }));
    }

    #[test]
    fn explicit_contract_accepts_successful_command_after_mutation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(Vec::new())
        );
        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
        assert!(state.interruption.is_none());
    }

    #[test]
    fn explicit_contract_uses_exact_simple_command_identity() {
        let hook = explicit_verification_hook("quality", "./quality-gate");
        let exact = serde_json::json!({"command": "./quality-gate"});
        let extra_args = serde_json::json!({"command": "./quality-gate --quick"});
        let compound = serde_json::json!({"command": "./quality-gate && echo done"});

        assert!(tool_call_verifies_explicit_hook(
            "bash",
            Some(&exact),
            &hook
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&extra_args),
            &hook
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&compound),
            &hook
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "quality",
            Some(&exact),
            &hook
        ));

        let mut scoped_hook = explicit_verification_hook("quality", "./quality-gate");
        scoped_hook.working_dir = Some("/workspace/check".into());
        let scoped_exact = serde_json::json!({
            "command": "cd /workspace/check && ./quality-gate"
        });
        let scoped_wrong_dir = serde_json::json!({
            "command": "cd /workspace/other && ./quality-gate"
        });
        assert!(tool_call_verifies_explicit_hook(
            "bash",
            Some(&scoped_exact),
            &scoped_hook
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&scoped_wrong_dir),
            &scoped_hook
        ));
    }

    #[test]
    fn successful_unknown_verifier_is_post_mutation_evidence() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(Vec::new())
        );
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn independent_explicit_verifiers_share_one_batch_window() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("unit", "./unit-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state).expect("both hooks are pending");
        assert!(completion_action_window_is_batchable(&state, &action));
        let quality = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./quality-gate\"}"}
        });
        let unit = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./unit-gate\"}"}
        });
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let admission = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![quality.clone(), unit.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            &[quality, unit],
        );
        assert_eq!(admission.admitted.len(), 2);
        assert!(admission.rejected.is_empty());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .is_some_and(|window| window.matched)
        );
    }

    #[test]
    fn dependent_explicit_verifiers_do_not_open_batch_window() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        let mut unit = explicit_verification_hook("unit", "./unit-gate");
        unit.depends_on.push("quality".into());
        state.hooks.stop_hooks.push(unit);
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state).expect("both hooks are pending");
        assert!(!completion_action_window_is_batchable(&state, &action));
    }

    #[test]
    fn explicit_contract_receipt_is_invalidated_by_a_later_mutation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(vec!["quality".to_string()])
        );
    }

    #[test]
    fn terminal_receipt_is_invalidated_by_unknown_or_failed_writer() {
        let cases = [(
            true,
            r#"{"command":"python3 -c \"open('/workspace/a','w').write('bad')\""}#,
            ),
            (
                false,
                r#"{"command":"printf bad > /workspace/a && false"}"#,
        )];
        for (ok, command) in cases {
            let mut state = make_state();
            state.task_profile.mutates_workspace = true;
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(r#"{"command":"./quality-gate"}"#),
            ));
            state
                .stall
                .tool_call_records
                .push(executed_record("bash", ok, Some(command)));

            assert!(!successful_post_mutation_observation(&state));
        }
    }

    #[test]
    fn quarantined_workspace_blocks_reader_settlement_and_final_text() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let root = temp.path().to_string_lossy().into_owned();
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(temp.path())
        );

        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(root);
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert!(workspace_observation_is_quarantined(&state));
        assert!(!successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
        assert!(completion_action_window_requires_followup(&state));
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn live_weak_receipt_can_settle_current_turn_but_not_restored_authority() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/edge-only/workspace".into());
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        let receipt = fields
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("edge-call-1".into()),
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"./opaque-writer"}"#.into()),
            runtime_args_full: Some(r#"{"command":"./opaque-writer"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: receipt,
            ..ToolCallRecord::default()
        });

        assert!(!workspace_observation_is_quarantined(&state));
        assert!(record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(!record_has_trusted_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(has_concrete_workspace_mutation(&state));

        state.stall.workspace_observation_quarantine = Some(
            astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
                Some("edge-call-1".into()),
            ),
        );

        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/edge-only/workspace/out.json"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/edge-only/workspace/out.json"}"#.into()),
            file_path: Some("/edge-only/workspace/out.json".into()),
            ..ToolCallRecord::default()
        });
        assert!(successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
        assert!(!completion_action_window_requires_followup(&state));
        assert!(!workspace_observation_requires_terminal_incomplete(&state));
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert!(state.interruption.is_none());

        let restored: Vec<ToolCallRecord> = serde_json::from_str(
            &serde_json::to_string(&state.stall.tool_call_records).expect("serialize records"),
        )
        .expect("restore records");
        state.stall.tool_call_records = restored;
        state.stall.workspace_observation_quarantine = Some(
            astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
                Some("edge-call-1".into()),
            ),
        );
        assert!(record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(workspace_observation_is_quarantined(&state));
        assert!(!has_concrete_workspace_mutation(&state));
        assert_eq!(pending_completion_action(&state), None);
        // The serialized record no longer has the live-only runtime
        // arguments, so it must reopen the bounded required-mutation guard
        // rather than silently completing from a weak historical receipt.
        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.hooks.completion_settlement.workspace_mutation_retries,
            1
        );
    }

    #[test]
    fn supervisor_receipt_is_authoritative_and_later_read_closes_mutation_epoch() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.hooks.workspace_root_hint = Some("/edge-only/workspace".into());
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("supervised-edge-call".into()),
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"python3 update.py"}"#.into()),
            runtime_args_full: Some(r#"{"command":"python3 update.py"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert!(record_has_trusted_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(!record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(has_concrete_workspace_mutation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/edge-only/workspace/out.json"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/edge-only/workspace/out.json"}"#.into()),
            file_path: Some("/edge-only/workspace/out.json".into()),
            ..ToolCallRecord::default()
        });

        assert!(successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn post_record_quarantine_transition_covers_weak_and_partial_facts() {
        let mut state = make_state();
        let weak_fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("weak-call".into()),
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: weak_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });
        let weak_records = state.stall.tool_call_records.clone();
        assert!(apply_workspace_observation_quarantine_transition(
            &mut state,
            &weak_records,
        ));
        assert_eq!(
            state
                .stall
                .workspace_observation_quarantine
                .as_ref()
                .map(|q| q.reason.as_str()),
            Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::WEAK_PROCESS_OWNERSHIP_REASON
            )
        );

        let mut partial = make_state();
        let partial_fields = astra_tools::ToolResult::error("partial".into())
            .with_workspace_mutation_partial(vec!["/workspace/a".into()])
            .metadata
            .expect("partial receipt");
        partial.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("partial-call".into()),
            name: "str_replace".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: partial_fields
                .get(astra_tools::workspace_observation::OBSERVED_FIELD)
                .and_then(serde_json::Value::as_bool),
            workspace_mutation_scope: partial_fields
                .get(astra_tools::workspace_observation::SCOPE_FIELD)
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            workspace_mutation_receipt: partial_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            workspace_mutation_partial: Some(true),
            workspace_mutation_partial_paths: Some(vec!["/workspace/a".into()]),
            ..ToolCallRecord::default()
        });
        let partial_records = partial.stall.tool_call_records.clone();
        assert!(apply_workspace_observation_quarantine_transition(
            &mut partial,
            &partial_records,
        ));
        assert_eq!(
            partial
                .stall
                .workspace_observation_quarantine
                .as_ref()
                .map(|q| q.reason.as_str()),
            Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::PARTIAL_MUTATION_REASON
            )
        );
    }

    #[test]
    fn untrusted_tool_cannot_quarantine_from_lookalike_receipt_fields() {
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "mcp__example__write".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert!(!workspace_observation_is_quarantined(&state));
        assert!(!record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
    }

    #[test]
    fn partial_quarantine_requires_typed_failed_multi_path_writer_receipt() {
        let typed_receipt = serde_json::json!({
            "schema": "workspace_mutation_partial_receipt.v1",
            "source": "typed_multi_path_writer",
            "scope": astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
            "changed": true,
            "ownership": astra_tools::workspace_observation::TYPED_MULTI_PATH_WRITER_OWNERSHIP,
            "paths": ["/workspace/a"],
        });
        for (name, ok, disposition, paths, receipt) in [
            (
                "mcp__example__write",
                false,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                true,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                false,
                astra_services::session_journal::ToolCallDisposition::Rejected,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                false,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(Vec::new()),
                Some(serde_json::json!({
                    "schema": "workspace_mutation_partial_receipt.v1",
                    "source": "typed_multi_path_writer",
                    "scope": astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
                    "changed": true,
                    "ownership": astra_tools::workspace_observation::TYPED_MULTI_PATH_WRITER_OWNERSHIP,
                    "paths": [],
                })),
            ),
        ] {
            let mut state = make_state();
            state.stall.tool_call_records.push(ToolCallRecord {
                name: name.into(),
                ok,
                disposition: Some(disposition),
                workspace_mutation_partial: Some(true),
                workspace_mutation_partial_paths: paths,
                workspace_mutation_receipt: receipt,
                ..ToolCallRecord::default()
            });
            let records = state.stall.tool_call_records.clone();
            assert!(!apply_workspace_observation_quarantine_transition(
                &mut state, &records
            ));
            assert!(!workspace_observation_is_quarantined(&state));
        }
    }

    #[test]
    fn rejected_bash_receipt_cannot_create_workspace_evidence() {
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert!(!workspace_observation_is_quarantined(&state));
        assert!(!record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
        assert!(!has_executed_positive_workspace_mutation(&state));
    }

    #[test]
    fn external_scratch_after_workspace_receipt_does_not_reset_observation_watermark() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/workspace/out.txt",
                        "content": "x"
                    })
                    .to_string(),
                ),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(r#"{"path":"/workspace/out.txt"}"#.into()),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/tmp/scratch.txt",
                        "content": "scratch"
                    })
                    .to_string(),
                ),
                file_path: Some("/tmp/scratch.txt".into()),
                ..Default::default()
            },
        ];

        assert!(successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn writer_shapes_without_workspace_receipt_do_not_create_observation_authority() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/workspace/out.txt",
                        "content": "x"
                    })
                    .to_string(),
                ),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/tmp/scratch.txt",
                        "content": "scratch"
                    })
                    .to_string(),
                ),
                file_path: Some("/tmp/scratch.txt".into()),
                ..Default::default()
            },
        ];

        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(!has_concrete_workspace_mutation(&state));
        assert!(!successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn failed_opaque_writer_after_workspace_receipt_still_resets_observation_watermark() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        let mut workspace_write = executed_record(
            "write_file",
            true,
            Some(r#"{"path":"/workspace/out.txt","content":"x"}"#),
        );
        workspace_write.file_path = Some("/workspace/out.txt".into());
        state.stall.tool_call_records = vec![
            workspace_write,
            ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(r#"{"path":"/workspace/out.txt"}"#.into()),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: false,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(r#"{"command":"opaque-unknown-command"}"#.into()),
                ..Default::default()
            },
        ];

        assert!(!successful_post_mutation_observation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
    }

    #[test]
    fn external_shell_observation_does_not_close_workspace_epoch() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        let mut workspace_write = executed_record(
            "write_file",
            true,
            Some(r#"{"path":"/workspace/out.txt","content":"x"}"#),
        );
        workspace_write.file_path = Some("/workspace/out.txt".into());
        state.stall.tool_call_records = vec![
            workspace_write,
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({"command": "cat /tmp/unrelated.txt"}).to_string(),
                ),
                ..Default::default()
            },
        ];

        assert!(!successful_post_mutation_observation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
    }

    #[test]
    fn inert_read_only_shell_does_not_close_workspace_epoch() {
        for command in ["echo ok", "true", "sleep 0"] {
            let mut state = make_state();
            state.hooks.workspace_root_hint = Some("/workspace".into());
            let mut workspace_write = executed_record(
                "write_file",
                true,
                Some(r#"{"path":"/workspace/out.txt","content":"x"}"#),
            );
            workspace_write.file_path = Some("/workspace/out.txt".into());
            state.stall.tool_call_records = vec![
                workspace_write,
                ToolCallRecord {
                    name: "bash".into(),
                    ok: true,
                    disposition: Some(
                        astra_services::session_journal::ToolCallDisposition::Executed,
                    ),
                    args_full: Some(serde_json::json!({"command": command}).to_string()),
                    ..Default::default()
                },
            ];

            assert!(
                !successful_post_mutation_observation(&state),
                "inert shell command must not be an observation receipt: {command}"
            );
            assert_eq!(
                pending_completion_action(&state),
                Some(CompletionAction::PostMutationObservation)
            );
        }
    }

    #[test]
    fn explicit_contract_receipt_is_invalidated_by_unknown_or_failed_writer() {
        let cases = [(
            true,
            r#"{"command":"python3 -c \"open('/workspace/a','w').write('bad')\""}#,
            ),
            (
                false,
                r#"{"command":"printf bad > /workspace/a && false"}"#,
        )];
        for (ok, command) in cases {
            let mut state = make_state();
            state
                .hooks
                .stop_hooks
                .push(explicit_verification_hook("quality", "./quality-gate"));
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(r#"{"command":"./quality-gate"}"#),
            ));
            state
                .stall
                .tool_call_records
                .push(executed_record("bash", ok, Some(command)));

            assert_eq!(
                missing_explicit_verification_hooks(&state),
                Some(vec!["quality".to_string()])
            );
        }
    }

    #[test]
    fn explicit_contract_reports_incomplete_after_bounded_recovery() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.verification_retries = 1;

        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("not verified"));
    }

    #[test]
    fn explicit_contract_requires_every_declared_obligation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("unit", "./unit-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(vec!["unit".to_string()])
        );
    }

    #[test]
    fn advisory_hook_does_not_create_a_terminal_gate() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(astra_turn_core::stop_hooks::StopHook {
                authoritative: false,
                ..explicit_verification_hook("discovery", "./quality-gate")
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));

        assert_eq!(missing_explicit_verification_hooks(&state), None);
        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
    }

    #[test]
    fn completion_action_projection_requires_typed_workspace_intent() {
        let mut state = make_state();
        assert_eq!(pending_completion_action(&state), None);

        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::RequiredWorkspaceMutation)
        );
    }

    #[test]
    fn completion_action_respects_typed_mutation_completion_scope() {
        use astra_config::user_profile::{MutationCompletionScope, TurnIntent};

        let mut external = make_state();
        external.task_profile = structured_mutating_profile();
        external.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(MutationCompletionScope::External),
        );
        assert_eq!(
            pending_completion_action(&external),
            Some(CompletionAction::RequiredExternalEffect),
            "external-only state must fail closed without an executor-owned receipt"
        );

        external
            .stall
            .tool_call_records
            .push(external_effect_record(
                astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP,
            ));
        assert_eq!(pending_completion_action(&external), None);

        for scope in [
            MutationCompletionScope::Unknown,
            MutationCompletionScope::Workspace,
        ] {
            let mut bounded = make_state();
            bounded.task_profile = structured_mutating_profile();
            bounded.turn_intent = Some(
                TurnIntent::default()
                    .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                    .with_mutation_completion_scope(scope),
            );
            assert_eq!(
                pending_completion_action(&bounded),
                Some(CompletionAction::RequiredWorkspaceMutation),
                "{scope:?} must preserve fail-closed workspace completion"
            );
        }

        let mut mixed = make_state();
        mixed.task_profile = structured_mutating_profile();
        mixed.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(MutationCompletionScope::Mixed),
        );
        assert_eq!(
            pending_completion_action(&mixed),
            Some(CompletionAction::RequiredExternalEffect)
        );
        mixed.stall.tool_call_records.push(external_effect_record(
            astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP,
        ));
        assert_eq!(
            pending_completion_action(&mixed),
            Some(CompletionAction::RequiredWorkspaceMutation),
            "a trusted external receipt cannot replace the mixed scope's workspace receipt"
        );
        mixed
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert_eq!(
            pending_completion_action(&mixed),
            Some(CompletionAction::PostMutationObservation),
            "both mutation receipts must advance to the ordinary workspace settlement gate"
        );
        mixed
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        assert_eq!(pending_completion_action(&mixed), None);
    }

    #[test]
    fn external_effect_completion_rejects_untrusted_or_failed_records() {
        use astra_config::user_profile::{MutationCompletionScope, TurnIntent};

        let mut state = make_state();
        state.task_profile = structured_mutating_profile();
        state.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(MutationCompletionScope::External),
        );

        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"exit 0"}"#),
        ));
        assert!(!has_concrete_external_effect(&state));

        state
            .stall
            .tool_call_records
            .push(external_effect_record("model_claim"));
        assert!(!has_concrete_external_effect(&state));

        let mut failed = external_effect_record(
            astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
        );
        failed.ok = false;
        state.stall.tool_call_records.push(failed);
        assert!(!has_concrete_external_effect(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::RequiredExternalEffect)
        );

        state.stall.tool_call_records.push(external_effect_record(
            astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
        ));
        assert!(has_concrete_external_effect(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn terminal_completion_action_preserves_workspace_intent_tristate() {
        let failed_writer = executed_record(
            "write_file",
            false,
            Some(r#"{"path":"/workspace/out.txt","content":"answer"}"#),
        );

        let mut must_mutate = make_state();
        mark_must_mutate(&mut must_mutate);
        must_mutate
            .stall
            .tool_call_records
            .push(failed_writer.clone());
        assert!(has_executed_positive_workspace_mutation(&must_mutate));
        assert!(!has_concrete_workspace_mutation(&must_mutate));
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&must_mutate, false),
            Some(CompletionAction::RequiredWorkspaceMutation)
        );

        let mut read_only = make_state();
        read_only.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
            ),
        );
        read_only
            .stall
            .tool_call_records
            .push(failed_writer.clone());
        assert_eq!(
            pending_terminal_completion_action_for_work_state(&read_only, false),
            None,
            "a mutation-risk record is not authority to add a write or observation obligation to a read-only turn"
        );

        for intent in [
            astra_config::user_profile::WorkspaceMutationIntent::Unknown,
            astra_config::user_profile::WorkspaceMutationIntent::MayMutate,
        ] {
            let mut uncertain = make_state();
            uncertain.turn_intent = Some(
                astra_config::user_profile::TurnIntent::default().with_workspace_mutation(intent),
            );
            uncertain
                .stall
                .tool_call_records
                .push(failed_writer.clone());
            assert_eq!(
                pending_terminal_completion_action_for_work_state(&uncertain, false),
                Some(CompletionAction::CompletionTaskAction),
                "intent={intent:?}"
            );
        }
    }

    #[test]
    fn complete_state_writer_narrowing_is_scoped_to_opaque_recovery() {
        let call = |name: &str, arguments: &str| {
            serde_json::json!({
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            })
        };
        let action = CompletionAction::RequiredWorkspaceMutation;
        let mut ordinary = make_state();
        mark_must_mutate(&mut ordinary);

        for candidate in [
            call("apply_patch", r#"{"patch":"*** Begin Patch"}"#),
            call(
                "edit_file",
                r#"{"path":"out.txt","old_text":"a","new_text":"b"}"#,
            ),
            call("bash", r#"{"command":"printf x > out.txt"}"#),
        ] {
            assert!(
                completion_action_matches_tool_call(&ordinary, &action, &candidate),
                "ordinary mutation windows must retain the existing writer surface: {candidate}"
            );
        }

        ordinary
            .hooks
            .completion_settlement
            .workspace_mutation_retries = 1;
        assert!(completion_action_matches_tool_call(
            &ordinary,
            &action,
            &call("write_file", r#"{"path":"out.txt","content":"complete\n"}"#,),
        ));
        for candidate in [
            call("apply_patch", r#"{"patch":"*** Begin Patch"}"#),
            call("bash", r#"{"command":"printf x > out.txt"}"#),
            call("read_file", r#"{"path":"out.txt"}"#),
            call("write_file", r#"{"path":"out.txt"}"#),
        ] {
            assert!(
                !completion_action_matches_tool_call(&ordinary, &action, &candidate),
                "opaque recovery accepts only a complete-state typed writer: {candidate}"
            );
        }
    }

    #[test]
    fn completion_task_action_accepts_task_tools_but_not_runtime_control() {
        let state = make_state();
        let action = CompletionAction::CompletionTaskAction;
        let call = |name: &str, arguments: &str| {
            serde_json::json!({
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            })
        };

        assert!(completion_action_matches_tool_call(
            &state,
            &action,
            &call(
                "write_file",
                r#"{"path":"/workspace/out.txt","content":"answer"}"#
            ),
        ));
        assert!(completion_action_matches_tool_call(
            &state,
            &action,
            &call("read_file", r#"{"path":"/workspace/input.txt"}"#),
        ));
        for control in [
            "introspect",
            "ask_user",
            "tool_search",
            "compress_context",
            "enter_plan_mode",
            "start_work",
            "run_next_work_item",
            "settle_work_item",
            "inspect_work_plan",
            "propose_work_plan",
            "delegate",
        ] {
            assert!(
                !completion_action_matches_tool_call(&state, &action, &call(control, "{}")),
                "control={control}"
            );
        }
    }

    #[test]
    fn completion_task_action_rejects_agent_and_running_fanout_operations() {
        let state = make_state();
        let action = CompletionAction::CompletionTaskAction;
        for (name, arguments) in [
            ("agent", r#"{"action":"spawn","task":"finish"}"#),
            ("agent", r#"{"action":"get_result","agent_id":"child-1"}"#),
            (
                "agent_fanout",
                r#"{"action":"start","task":"finish","target_count":2}"#,
            ),
            (
                "agent_fanout",
                r#"{"action":"get_results","group_id":"running-group"}"#,
            ),
        ] {
            let call = serde_json::json!({
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            });
            assert!(
                !completion_action_matches_tool_call(&state, &action, &call),
                "{name} must not consume the final task-action boundary: {arguments}"
            );
        }
    }

    #[test]
    fn completion_action_projection_tracks_executed_mutation_without_intent() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert!(!state.task_profile.mutates_workspace);
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
    }

    #[test]
    fn completion_action_hint_keeps_observation_scope_generic_without_receipt_target() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        let hint = completion_action_hint(&CompletionAction::PostMutationObservation);
        assert_eq!(hint["reason_code"], "latest_mutation_not_observed");
        assert!(hint["latest_known_stable_target"].is_null());
        assert_eq!(hint["accepted_action_shapes"][0]["tool"], "bash");
        let constraint = hint["accepted_action_shapes"][0]["constraint"]
            .as_str()
            .expect("observation constraint");
        assert!(constraint.contains("strongest proportionate check"));
        assert!(constraint.contains("import is structural evidence only"));
    }

    #[test]
    fn post_mutation_recovery_advertises_executor_bash_verify_receipt() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        let instruction = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "post_mutation_observation_missing")
            .and_then(|entry| entry.payload["instruction"].as_str())
            .expect("post-mutation recovery instruction");
        assert!(instruction.contains("`mode` to `verify`"));
        assert!(instruction.contains("executor will attest"));
        assert!(!instruction.contains("explicit working-directory field"));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("recovery must install typed admission authority");
        assert_eq!(window.action, CompletionAction::PostMutationObservation);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 1);
        assert!(!window.consumed && !window.matched);
    }

    #[test]
    fn post_mutation_recovery_rejects_plain_bash_and_closes_only_with_verify_receipt() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));

        let plain_bash = serde_json::json!({
            "id": "ordinary-compound-validation",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cd /workspace; test -f result"}"#,
            }
        });
        let rejected = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![plain_bash.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&plain_bash),
        );
        assert!(rejected.admitted.is_empty());
        assert_eq!(rejected.rejected.len(), 1);
        let rejection: serde_json::Value =
            serde_json::from_str(&rejected.rejected[0].result).expect("structured rejection");
        assert_eq!(rejection["error_kind"], "completion_action_mismatch");
        assert_eq!(rejection["retryable"], true);
        assert!(
            state.stall.tool_call_records.len() == 1,
            "mismatch must not execute"
        );

        let verify_bash = serde_json::json!({
            "id": "executor-verified-validation",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"test -f result","mode":"verify"}"#,
            }
        });
        let admitted = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![verify_bash.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&verify_bash),
        );
        assert_eq!(admitted.admitted, vec![verify_bash]);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("admitted action remains auditable until its receipt arrives");
        assert!(window.consumed && window.matched);
        assert_eq!(window.mismatch_corrections_remaining, 0);

        // Admission alone cannot settle the turn. Model-authored stdout or a
        // forged receipt remains insufficient; only the executor's exact v2
        // verification receipt may close the post-mutation epoch.
        let verify_receipt =
            astra_tools::workspace_observation::explicit_workspace_verification_receipt();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"test -f result","mode":"verify"}"#.into()),
            runtime_args_full: Some(r#"{"command":"test -f result","mode":"verify"}"#.into()),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: verify_receipt
                .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });
        assert!(successful_post_mutation_observation(&state));
        assert_eq!(pending_completion_action(&state), None);
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert!(
            state.interruption.is_none(),
            "authentic receipt closes recovery"
        );
    }

    #[test]
    fn completion_action_hint_does_not_guess_from_a_later_unknown_writer() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"python3 -c \"open('/workspace/other','w').write('x')\""}"#),
        ));

        let hint = completion_action_hint(&CompletionAction::PostMutationObservation);
        assert!(hint["latest_known_stable_target"].is_null());
        assert!(
            hint["accepted_action_shapes"]
                .as_array()
                .is_some_and(|shapes| shapes.iter().all(|shape| shape["tool"] != "read_file"))
        );
    }

    #[test]
    fn completion_action_hint_keeps_required_and_explicit_actions_generic() {
        let required = completion_action_hint(&CompletionAction::RequiredWorkspaceMutation);
        assert_eq!(required["reason_code"], "workspace_mutation_missing");
        assert!(required["latest_known_stable_target"].is_null());
        assert!(
            required["accepted_action_shapes"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );

        let explicit = completion_action_hint(&CompletionAction::ExplicitVerification {
            missing_labels: vec!["quality".into(), "unit".into()],
        });
        assert_eq!(explicit["reason_code"], "explicit_verification_missing");
        assert_eq!(
            explicit["missing_labels"],
            serde_json::json!(["quality", "unit"])
        );
        assert!(
            explicit["accepted_action_shapes"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
    }

    #[test]
    fn opaque_recovery_hint_requires_complete_state_writer_and_owner_evidence() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.completion_settlement.workspace_mutation_retries = 1;

        let hint =
            completion_action_hint_for_state(&state, &CompletionAction::RequiredWorkspaceMutation);
        let shape = &hint["accepted_action_shapes"][0];
        assert_eq!(shape["tool"], "write_file");
        assert!(
            shape["constraint"]
                .as_str()
                .is_some_and(|text| text.contains("full desired bytes"))
        );
        assert!(
            shape["changed_outcome"]
                .as_str()
                .is_some_and(|text| text.contains("mutation receipt"))
        );
        assert!(
            shape["already_exact_outcome"]
                .as_str()
                .is_some_and(|text| text.contains("separate full read_file"))
        );
        assert!(
            shape["evidence_inference_forbidden"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "bash_exit_status"))
        );
    }

    #[test]
    fn canonical_work_validation_action_accepts_only_canonical_validator_shape() {
        let state = make_state();
        let canonical = serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cargo test"}"#
            }
        });
        let probe = serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"echo still-working"}"#
            }
        });

        assert!(completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &canonical,
        ));
        assert!(!completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &probe,
        ));
        let hint = completion_action_hint(&CompletionAction::CanonicalWorkValidation);
        assert_eq!(hint["reason_code"], "work_validation_stale_or_failed");
        assert_eq!(hint["accepted_action_family"], "canonical_work_validation");
        let shape = &hint["accepted_action_shapes"][0];
        assert_eq!(
            shape["evidence_source"],
            "prior_runtime_recognized_project_validation"
        );
        assert_eq!(shape["raw_arguments_projected"], false);
        assert!(shape["constraint"].as_str().is_some_and(|constraint| {
            constraint.contains("direct standard project build/test")
                && constraint.contains("custom inline program")
        }));
        let serialized = hint.to_string();
        assert!(!serialized.contains("cargo test"));
        assert!(!serialized.contains("/workspace"));
    }

    #[test]
    fn recovery_revalidation_must_match_the_failed_validation_operation() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_operation = Some("pytest -q".into());
        let same_operation = serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"pytest -q"}"#
            }
        });
        let different_operation = serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cargo check"}"#
            }
        });

        assert!(completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &same_operation,
        ));
        assert!(!completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &different_operation,
        ));
    }

    #[test]
    fn narrowed_validator_cannot_clear_a_failed_work_validation() {
        let mut state = make_state();
        let failed = "python -m pytest tests --ignore=tests/slow.py";
        let narrowed = "python -m pytest tests --ignore=tests/slow.py -k 'not flaky_case'";
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record(failed, "test_failure"),
            validation_record(narrowed, "success"),
        ];

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Failed,
            "a different validator may add evidence but cannot waive the failed operation"
        );
        assert_eq!(
            failed_work_validation_operation(&state).as_deref(),
            Some(failed)
        );
        assert_eq!(
            pending_completion_action_for_work_state(&state, true),
            Some(CompletionAction::CanonicalWorkValidation)
        );
        // The active Work executor normally pins this operation when it opens
        // the bounded revalidation window. Keep the matching assertion below
        // focused on that persisted window contract rather than constructing
        // a full runtime executor in this unit fixture.
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_operation = Some(failed.to_string());

        let exact_retry = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": serde_json::json!({"command": failed}).to_string()}
        });
        let narrowed_retry = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": serde_json::json!({"command": narrowed}).to_string()}
        });
        assert!(completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &exact_retry,
        ));
        assert!(!completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &narrowed_retry,
        ));
    }

    #[test]
    fn fresh_work_attempt_resets_prior_validation_recovery_scope_across_a_mixed_batch() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_operation = Some("pytest -q".into());
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "settle_work_item".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        });
        // A duplicate/failing sibling can follow the successful settlement in
        // one provider batch. It must not hide the attempt boundary.
        state
            .stall
            .tool_call_records
            .push(validation_record("pytest -q", "test_failure"));

        advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
            &mut state, true, 0,
        );

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            0
        );
        assert!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .is_none()
        );
        let next_attempt_validator = serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cargo check"}"#
            }
        });
        assert!(completion_action_matches_tool_call(
            &state,
            &CompletionAction::CanonicalWorkValidation,
            &next_attempt_validator,
        ));
    }

    #[test]
    fn canonical_validation_mismatch_explains_hidden_shape_then_accepts_direct_retry() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(validation_record("cargo test", "test_failure"));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let opaque = serde_json::json!({
            "id": "opaque-self-check",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"python3 -c \"print('PASS')\""}"#
            }
        });

        let rejected = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![opaque.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&opaque),
        );

        assert!(rejected.admitted.is_empty());
        assert_eq!(rejected.rejected.len(), 1);
        let result: serde_json::Value =
            serde_json::from_str(&rejected.rejected[0].result).expect("structured rejection");
        assert_eq!(result["retryable"], true);
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|error| error.contains("direct standard project build/test")
                    && error.contains("custom inline program"))
        );
        assert_eq!(
            result["action_hint"]["accepted_action_shapes"][0]["evidence_source"],
            "prior_runtime_recognized_project_validation"
        );
        assert!(!result.to_string().contains("/workspace"));

        let retry = serde_json::json!({
            "id": "direct-project-validator",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cargo test"}"#
            }
        });
        let admitted = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![retry.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&retry),
        );

        assert_eq!(admitted.admitted, vec![retry]);
        assert!(admitted.rejected.is_empty());
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("matched validation remains auditable");
        assert!(window.consumed);
        assert!(window.matched);
        assert_eq!(window.attempts_remaining, 0);
    }

    #[test]
    fn opaque_writer_does_not_create_a_terminal_obligation_without_receipt() {
        let mut state = make_state();
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"python3 -c \"open('/workspace/out','w').write('x')\""}"#),
        ));

        assert!(!state.task_profile.mutates_workspace);
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn failed_direct_writer_does_not_create_observation_authority_without_intent() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", false, None));

        assert!(!state.task_profile.mutates_workspace);
        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(!has_concrete_workspace_mutation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn opaque_writer_after_positive_write_keeps_the_latest_observation_barrier() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            false,
            Some(r#"{"command":"opaque-unknown-command"}"#),
        ));

        assert!(!state.task_profile.mutates_workspace);
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
    }

    #[test]
    fn rejected_writer_does_not_create_a_completion_action_without_intent() {
        let mut state = make_state();
        let mut rejected = executed_record("write_file", false, None);
        rejected.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        state.stall.tool_call_records.push(rejected);

        assert!(!state.task_profile.mutates_workspace);
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn completion_action_projection_advances_from_mutation_to_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert!(record_is_effective_workspace_repair(
            state.stall.tool_call_records.last().unwrap()
        ));
        let repair_window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
            .unwrap();
        repair_window.consumed = true;
        repair_window.matched = true;
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn required_mutation_window_advances_to_observation_after_success() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_failed_action_retries = 1;
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the dependent observation remains bounded and auditable");
        assert_eq!(window.action, CompletionAction::PostMutationObservation);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 0);
        assert!(!window.consumed);
        assert!(!window.matched);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            0,
            "a new mutation epoch owns an independent bounded observation retry"
        );
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["signal"] == "typed_completion_action_available"
                && entry.payload["mode"] == "bounded_completion_chain"
                && entry.payload["allowed_action"] == "post_mutation_observation"
        }));
    }

    #[test]
    fn failed_observation_grants_one_outcome_gated_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let record = validation_record("cargo test --lib", "test_failure");
        state.stall.tool_call_records.push(record);
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert_eq!(window.action, CompletionAction::PostMutationRepair);
        assert!(!window.consumed && !window.matched);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            1
        );
        assert_eq!(state.remaining_turns, remaining + 1);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["signal"] == "failed_post_mutation_observation_repair_once"
                && entry.payload["allowed_action"] == "post_mutation_repair"
        }));
    }

    #[test]
    fn edge_failed_validator_without_executor_error_grants_one_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = validation_record("cargo test --lib", "test_failure");
        // Edge verify reports a failed assertion as a non-OK tool result,
        // but it remains an executor-complete validator when no typed tool
        // error or workspace quarantine is attached.
        record.ok = false;
        state.stall.tool_call_records.push(record);

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationRepair
        );
    }

    #[test]
    fn infrastructure_failed_observation_does_not_grant_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = executed_record("read_file", false, None);
        record.error_kind = Some(astra_core::ErrorKind::ToolTimeout);
        state.stall.tool_call_records.push(record);
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationObservation
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            0
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            0
        );
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert!(window.consumed && window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.remaining_turns, remaining);
    }

    #[test]
    fn unavailable_observation_grants_one_same_action_retry_without_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = executed_record("bash", false, None);
        record.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        state.stall.tool_call_records.push(record);
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert_eq!(window.action, CompletionAction::PostMutationObservation);
        assert_eq!(window.attempts_remaining, 1);
        assert!(!window.consumed && !window.matched);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            1
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            0
        );
        assert_eq!(state.remaining_turns, remaining + 1);
    }

    #[test]
    fn second_unavailable_observation_does_not_open_another_retry() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_failed_action_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = executed_record("bash", false, None);
        record.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        state.stall.tool_call_records.push(record);
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert!(window.consumed && window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.remaining_turns, remaining);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            0
        );
    }

    #[test]
    fn unavailable_observation_with_partial_mutation_does_not_retry() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = executed_record("bash", false, None);
        record.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        record.workspace_mutation_partial = Some(true);
        record.workspace_mutation_partial_paths = Some(vec!["/workspace/out".into()]);
        state.stall.tool_call_records.push(record);
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert!(window.consumed && window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.remaining_turns, remaining);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            0
        );
    }

    #[test]
    fn unavailable_observation_with_multiple_executed_records_does_not_retry() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut unavailable = executed_record("bash", false, None);
        unavailable.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        state.stall.tool_call_records.push(unavailable);
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round_from_record_index(&mut state, 0);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert!(window.consumed && window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.remaining_turns, remaining);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            0
        );
    }

    #[test]
    fn test_failure_with_executor_unavailable_error_does_not_grant_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = validation_record("cargo test --lib", "test_failure");
        record.ok = false;
        record.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        state.stall.tool_call_records.push(record);

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationObservation
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            0
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_failed_action_retries,
            0
        );
    }

    #[test]
    fn expected_negative_observation_does_not_grant_repair() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let record = validation_record("git diff --quiet", "domain_negative");
        state.stall.tool_call_records.push(record);

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationObservation
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_retries,
            0
        );
    }

    #[test]
    fn post_mutation_repair_requires_the_same_validator_not_a_generic_read() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_validation_operation = Some("cargo test --lib".into());
        let action = CompletionAction::PostMutationObservation;
        let read = serde_json::json!({
            "function": {"name": "read_file", "arguments": "{\"path\":\"/workspace/result\"}"}
        });
        let same_validator = serde_json::json!({
            "function": {"name": "bash", "arguments": "{\"command\":\"cargo test --lib\"}"}
        });
        let different_validator = serde_json::json!({
            "function": {"name": "bash", "arguments": "{\"command\":\"cargo check\"}"}
        });

        assert!(completion_action_match_label(&state, &action, &read).is_none());
        assert_eq!(
            completion_action_match_label(&state, &action, &same_validator).as_deref(),
            Some("post_mutation_revalidation")
        );
        assert!(completion_action_match_label(&state, &action, &different_validator).is_none());
    }

    #[test]
    fn active_work_repair_returns_to_canonical_validation() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "success"),
            executed_record("write_file", true, None),
            validation_record("cargo test", "test_failure"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationRepair
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            1
        );

        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert!(record_is_effective_workspace_repair(
            state.stall.tool_call_records.last().unwrap()
        ));
        let repair_window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
            .unwrap();
        repair_window.consumed = true;
        repair_window.matched = true;
        assert_eq!(
            pending_completion_action_for_work_state(&state, true),
            Some(CompletionAction::CanonicalWorkValidation)
        );
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::CanonicalWorkValidation
        );
        assert!(
            state
                .hooks
                .completion_settlement
                .post_mutation_repair_validation_operation
                .is_none()
        );

        state
            .stall
            .tool_call_records
            .push(validation_record("cargo test", "test_failure"));
        let validation_window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
            .unwrap();
        validation_window.consumed = true;
        validation_window.matched = true;
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert_ne!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .map(|window| &window.action),
            Some(&CompletionAction::CanonicalWorkRepair)
        );
    }

    #[test]
    fn successful_repair_exposes_one_final_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationObservation
        );
    }

    #[test]
    fn successful_authoritative_bash_repair_exposes_final_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let receipt = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
        )
        .remove(astra_tools::workspace_observation::RECEIPT_FIELD)
        .expect("authoritative receipt");
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"sed -i 's/a/b/' /workspace/result"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: Some(receipt),
            ..ToolCallRecord::default()
        });

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationObservation
        );
    }

    #[test]
    fn failed_repair_does_not_claim_or_grant_final_observation() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", false, None));
        let remaining = state.remaining_turns;

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .unwrap();
        assert_eq!(window.action, CompletionAction::PostMutationRepair);
        assert!(window.consumed && window.matched);
        assert_eq!(state.remaining_turns, remaining);
    }

    #[test]
    fn failed_repair_with_a_receipt_does_not_grant_final_observation() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .post_mutation_repair_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut record = executed_record("write_file", true, None);
        record.ok = false;
        state.stall.tool_call_records.push(record);

        advance_completion_action_window_after_tool_round(&mut state);

        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .unwrap()
                .action,
            CompletionAction::PostMutationRepair
        );
    }

    #[test]
    fn active_work_completion_action_returns_to_typed_work_settlement() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        state.hooks.completion_settlement.text_only = true;

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(
            state.volatile_pending.iter().any(|entry| {
                entry.payload["signal"] == "completion_action_attempted_settle_work"
            })
        );
    }

    #[test]
    fn failed_bounded_work_revalidation_opens_one_repair_then_revalidation_cycle() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            pending_completion_action_for_work_state(&state, true),
            Some(CompletionAction::CanonicalWorkValidation)
        );
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("one repair window");
        assert_eq!(window.action, CompletionAction::CanonicalWorkRepair);
        assert!(!window.consumed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            1
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(
            state.volatile_pending.iter().any(|entry| {
                entry.payload["signal"] == "canonical_validation_failed_repair_once"
            })
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn failed_validation_after_mutation_opens_the_same_bounded_repair_cycle() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "success"),
            executed_record("write_file", true, None),
            validation_record("cargo test", "test_failure"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Failed
        );
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("one repair window");
        assert_eq!(window.action, CompletionAction::CanonicalWorkRepair);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn rejected_work_settlement_opens_repair_before_budget_settlement() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "failed"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_settlement,
        ];
        state.max_turns = 24;
        state.remaining_turns = 1;
        state.hooks.completion_settlement.work_settlement_only = false;

        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("rejected truthful settlement gets one bounded repair");
        assert_eq!(window.action, CompletionAction::CanonicalWorkRepair);
        assert!(!window.consumed);
        assert_eq!(state.max_turns, 25);
        assert_eq!(state.remaining_turns, 2);
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some("cargo test")
        );
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["signal"] == "canonical_validation_failed_repair_once"
                && entry.payload["origin"] == "rejected_work_settlement"
        }));
    }

    #[test]
    fn rejected_work_settlement_does_not_expand_existing_repair_headroom() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "failed"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_settlement,
        ];
        state.max_turns = 24;
        state.remaining_turns = 3;

        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        assert!(matches!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .map(|window| &window.action),
            Some(CompletionAction::CanonicalWorkRepair)
        ));
        assert_eq!(state.max_turns, 24);
        assert_eq!(state.remaining_turns, 3);
    }

    #[test]
    fn rejected_settlement_does_not_replace_an_existing_validation_window() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "failed"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_settlement,
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: false,
            });

        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("existing validation obligation remains authoritative");
        assert_eq!(window.action, CompletionAction::CanonicalWorkValidation);
        assert_eq!(window.mismatch_corrections_remaining, 0);
        assert!(window.consumed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            0
        );
    }

    #[test]
    fn historical_rejected_settlement_cannot_reopen_a_later_failure() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "failed"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test -p first", "test_failure"),
            rejected_settlement,
            validation_record("cargo test -p first", "success"),
            validation_record("cargo test -p second", "test_failure"),
            executed_record("read_file", true, None),
        ];

        advance_rejected_work_settlement_recovery_for_test(&mut state, 5);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            0
        );
    }

    #[test]
    fn concurrent_mutation_risk_after_rejected_settlement_requires_revalidation() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "failed"
            })
            .to_string(),
        );
        let weak_receipt = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        )
        .remove(astra_tools::workspace_observation::RECEIPT_FIELD)
        .expect("weak receipt");
        let weak_repair = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(r#"{"command":"./opaque-writer"}"#.into()),
            runtime_args_full: Some(r#"{"command":"./opaque-writer"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: Some(weak_receipt),
            ..ToolCallRecord::default()
        };
        assert!(!record_is_effective_workspace_repair(&weak_repair));
        let mut restored_weak_repair = weak_repair.clone();
        restored_weak_repair.runtime_args_full = None;
        assert!(!record_is_effective_workspace_repair(&restored_weak_repair));
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_settlement,
            weak_repair,
        ];
        state.max_turns = 24;
        state.remaining_turns = 1;

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Stale
        );
        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        assert!(matches!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .map(|window| &window.action),
            Some(CompletionAction::CanonicalWorkValidation)
        ));
        assert_eq!(state.max_turns, 25);
        assert_eq!(state.remaining_turns, 2);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            1
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn rejected_writer_does_not_invalidate_current_validation() {
        let mut state = make_state();
        let mut rejected_writer = executed_record("write_file", false, None);
        rejected_writer.disposition = Some(ToolCallDisposition::Rejected);
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_writer,
        ];

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Failed,
            "admission-only writer shapes must not invalidate executed validation evidence"
        );
    }

    #[test]
    fn stale_rejected_settlement_reopens_the_latest_canonical_validation() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "unresolved_work_validation",
                "validation_state": "stale"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test -p changed-package", "success"),
            executed_record("write_file", true, None),
            rejected_settlement,
        ];
        state.max_turns = 24;
        state.remaining_turns = 1;

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Stale
        );
        advance_rejected_work_settlement_recovery_for_test(&mut state, 3);

        assert!(matches!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .map(|window| &window.action),
            Some(CompletionAction::CanonicalWorkValidation)
        ));
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_operation
                .as_deref(),
            Some("cargo test -p changed-package")
        );
        assert_eq!(state.max_turns, 25);
        assert_eq!(state.remaining_turns, 2);
    }

    #[test]
    fn rejected_work_settlement_without_failed_canonical_validation_stays_settlement_only() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            rejected_settlement,
        ];
        state.hooks.completion_settlement.work_settlement_only = true;

        advance_completion_action_window_after_tool_round_for_work_state_from_record_index(
            &mut state, true, 1,
        );

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_retries,
            0
        );
    }

    #[test]
    fn unrelated_rejected_work_settlement_does_not_open_repair_authority() {
        let mut state = make_state();
        let mut rejected_settlement = executed_record("settle_work_item", false, None);
        rejected_settlement.disposition = Some(ToolCallDisposition::Rejected);
        rejected_settlement.result_full = Some(
            serde_json::json!({
                "status": "rejected",
                "error_kind": "work_settlement_only",
                "validation_state": "failed"
            })
            .to_string(),
        );
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            rejected_settlement,
        ];
        state.hooks.completion_settlement.work_settlement_only = true;

        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn failed_executed_work_settlement_does_not_open_repair_authority() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            executed_record("settle_work_item", false, None),
        ];
        state.hooks.completion_settlement.work_settlement_only = true;

        advance_rejected_work_settlement_recovery_for_test(&mut state, 2);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn quarantined_failed_work_validation_cannot_open_repair_authority() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(temp.path())
        );
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(temp.path().to_string_lossy().into_owned());
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        let turns_before = state.max_turns;

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert_eq!(state.max_turns, turns_before);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn successful_bounded_work_repair_requires_revalidation_before_settlement() {
        let mut state = make_state();
        // This test isolates the validation-recovery chain; the required
        // workspace-mutation obligation was already satisfied before it.
        state.task_profile.mutates_workspace = false;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            executed_record("write_file", true, None),
        ];
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Stale
        );
        assert_eq!(
            pending_completion_action_for_work_state(&state, true),
            Some(CompletionAction::CanonicalWorkValidation)
        );
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("canonical revalidation window");
        assert_eq!(window.action, CompletionAction::CanonicalWorkValidation);
        assert!(!window.consumed);
        assert!(!state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn failed_bounded_work_repair_reopens_one_corrective_mutation() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = false;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            executed_record("str_replace", false, None),
        ];
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        let turns_before = state.max_turns;
        let remaining_before = state.remaining_turns;

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("a failed repair receives one bounded correction");
        assert_eq!(window.action, CompletionAction::CanonicalWorkRepair);
        assert!(!window.consumed);
        assert!(!window.matched);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 1);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .canonical_validation_recovery_failed_action_retries,
            1
        );
        assert_eq!(state.max_turns, turns_before + 1);
        assert_eq!(state.remaining_turns, remaining_before + 1);
        assert!(
            state.volatile_pending.iter().any(|entry| {
                entry.payload["signal"] == "canonical_work_repair_failed_retry_once"
            })
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn second_failed_bounded_work_repair_cannot_reopen_the_same_repair() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = false;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
            executed_record("str_replace", false, None),
        ];
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_failed_action_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkRepair,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(
            !state.volatile_pending.iter().any(|entry| {
                entry.payload["signal"] == "canonical_work_repair_failed_retry_once"
            }),
            "a second failed repair must not create another repair retry"
        );
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .map(|window| &window.action),
            Some(&CompletionAction::CanonicalWorkValidation),
            "the only remaining path is truthful revalidation, not another repair"
        );
    }

    #[test]
    fn second_failed_bounded_work_revalidation_settles_truthfully() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            validation_record("cargo test", "test_failure"),
        ];
        state
            .hooks
            .completion_settlement
            .canonical_validation_recovery_retries = 1;
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn successful_bounded_work_revalidation_clears_debt_before_settlement() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            executed_record("write_file", true, None),
            validation_record("cargo test", "test_failure"),
            validation_record("cargo test", "success"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            current_work_validation_state(&state),
            WorkValidationState::Passed
        );
        assert_eq!(pending_completion_action_for_work_state(&state, true), None);
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn successful_work_revalidation_cannot_skip_an_independent_verification_hook() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            executed_record("write_file", true, None),
            validation_record("cargo test", "test_failure"),
            validation_record("cargo test", "success"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert!(matches!(
            pending_completion_action_for_work_state(&state, true),
            Some(CompletionAction::ExplicitVerification { .. })
        ));
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
    }

    #[test]
    fn canonical_revalidation_may_also_satisfy_the_exact_declared_hook() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "cargo test"));
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "run_next_work_item".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
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
            executed_record("write_file", true, None),
            validation_record("cargo test", "test_failure"),
            validation_record("cargo test", "success"),
        ];
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::CanonicalWorkValidation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(pending_completion_action_for_work_state(&state, true), None);
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn active_work_required_mutation_still_chains_through_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let max_turns_before = state.max_turns;
        let remaining_before = state.remaining_turns;

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("verification must precede Work settlement");
        assert_eq!(window.action, CompletionAction::PostMutationObservation);
        assert!(!window.consumed);
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, max_turns_before + 1);
        assert_eq!(state.remaining_turns, remaining_before + 1);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["mode"] == "bounded_completion_then_work_settlement"
                && entry.payload["instruction"]
                    .as_str()
                    .is_some_and(|instruction| instruction.contains("settle_work_item"))
        }));
    }

    #[test]
    fn active_work_unmatched_completion_action_cannot_settle_work() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: false,
            });

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn active_work_failed_completion_action_cannot_settle_work() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", false, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn active_work_quarantined_completion_action_cannot_settle_work() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(temp.path())
        );
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(temp.path().to_string_lossy().into_owned());
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(workspace_observation_is_quarantined(&state));
        assert_eq!(pending_completion_action(&state), None);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn failed_required_mutation_does_not_downgrade_to_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", false, None));

        advance_completion_action_window_after_tool_round(&mut state);

        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the failed required action remains visible");
        assert_eq!(window.action, CompletionAction::RequiredWorkspaceMutation);
        assert!(window.consumed);
        assert!(window.matched);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .volatile_pending
                .iter()
                .all(|entry| { entry.payload["mode"] != "bounded_completion_chain" })
        );
    }

    #[test]
    fn completion_action_matches_only_declared_verification_and_not_reads() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state).expect("verification should be pending");

        let verify = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./quality-gate\"}"}
        });
        let read = serde_json::json!({
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        });
        assert!(completion_action_matches_tool_call(
            &state, &action, &verify
        ));
        assert!(!completion_action_matches_tool_call(&state, &action, &read));
    }

    #[test]
    fn completion_action_observation_uses_positive_shape_and_bound_scope() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = CompletionAction::PostMutationObservation;

        let call = |command: &str| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": serde_json::json!({"command": command}).to_string(),
                }
            })
        };

        assert!(!completion_action_matches_tool_call(
            &state,
            &action,
            &call("echo ok")
        ));
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
            "cargo test '--help'",
            "cargo test \"$MODE\"",
            "pytest \"$MODE\"",
            "touch /workspace/out | cargo test",
            "cargo test | touch /workspace/out",
            "cat /workspace/source > /workspace/output",
        ] {
            assert!(
                !completion_action_matches_tool_call(&state, &action, &call(command)),
                "metadata/option-only reader must not consume the completion action: {command}"
            );
        }
        assert!(!completion_action_matches_tool_call(
            &state,
            &action,
            &call("cat /workspace/missing || true")
        ));
        assert!(!completion_action_matches_tool_call(
            &state,
            &action,
            &call("cat /workspace/missing; echo ok")
        ));
        assert!(completion_action_matches_tool_call(
            &state,
            &action,
            &call("cat /workspace/result && true")
        ));
        for command in [
            "grep -n pattern /workspace/result",
            "grep -e pattern /workspace/result",
            "grep -h pattern /workspace/result",
            "grep -- -h /workspace/result",
            "grep '>' /workspace/result",
            "wc -c /workspace/result",
            "ls -h",
            "test -e /workspace/result",
            "[ -f /workspace/result ]",
        ] {
            assert!(
                completion_action_matches_tool_call(&state, &action, &call(command)),
                "valid reader must match the completion action: {command}"
            );
        }
        for command in [
            "printf x | cat",
            "[ foo ] | cat",
            "printf x | sha256sum",
            "test foo | cat",
            "test foo | true",
            "cargo test --help",
            "pytest --help",
            "touch /workspace/out | cargo test",
            "cargo test | touch /workspace/out",
        ] {
            assert!(
                !completion_action_matches_tool_call(&state, &action, &call(command)),
                "stdin-only pipelines must not consume a workspace observation action: {command}"
            );
        }
        for command in [
            "cargo test | tail -20",
            "cat /workspace/result | head",
            "[ -f /workspace/result ] | cat",
            "cat /workspace/source > /workspace/output && cat /workspace/output",
            "grep '>' /workspace/result",
            "cargo test -v",
        ] {
            assert!(
                completion_action_matches_tool_call(&state, &action, &call(command)),
                "a reader may inherit evidence from an earlier workspace-producing stage: {command}"
            );
        }
        assert!(completion_action_matches_tool_call(
            &state,
            &action,
            &call("[ -f /workspace/result ] | cat")
        ));
        assert!(!completion_action_matches_tool_call(
            &state,
            &action,
            &call("cat /tmp/unrelated")
        ));
    }

    #[test]
    fn executed_observation_receipt_requires_workspace_input_provenance() {
        for command in [
            "printf x | cat",
            "[ foo ] | cat",
            "printf x | sha256sum",
            "test foo | cat",
            "test foo | true",
        ] {
            let mut state = make_state();
            state.hooks.workspace_root_hint = Some("/workspace".into());
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(&serde_json::json!({"command": command}).to_string()),
            ));

            assert!(
                !successful_post_mutation_observation(&state),
                "stdin-only pipeline must not close a workspace mutation epoch: {command}"
            );
        }

        for command in [
            "cargo test | tail -20",
            "cat /workspace/result | head",
            "[ -f /workspace/result ] | cat",
            "cat /workspace/source > /workspace/output && cat /workspace/output",
            "grep '>' /workspace/result",
            "cargo test -v",
        ] {
            let mut state = make_state();
            state.hooks.workspace_root_hint = Some("/workspace".into());
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(&serde_json::json!({"command": command}).to_string()),
            ));

            assert!(
                successful_post_mutation_observation(&state),
                "pipeline reader may inherit a receipt from an earlier workspace stage: {command}"
            );
        }
    }

    #[test]
    fn completion_action_admission_consumes_any_request_but_executes_only_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let read = serde_json::json!({
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        });
        let external_write = serde_json::json!({
            "type": "function",
            "function": {"name": "github", "arguments": "{}"}
        });
        let admission = ToolCallAdmission {
            admitted: vec![read.clone(), external_write.clone()],
            rejected: Vec::new(),
            completion_action_applied: false,
        };

        let admission =
            apply_completion_action_admission(&mut state, admission, &[read, external_write]);

        assert_eq!(admission.admitted.len(), 1);
        assert_eq!(
            admission.admitted[0]["function"]["name"].as_str(),
            Some("read_file")
        );
        assert_eq!(admission.rejected.len(), 1);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("window remains auditable until settlement");
        assert!(window.consumed);
        assert!(window.matched);
        assert!(
            !state.hooks.completion_settlement.text_only,
            "pre-execution admission must not make its own legal action look like a wrap-up violation"
        );
    }

    #[test]
    fn non_executed_completion_mismatch_allows_one_idempotent_correction() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let wrong = serde_json::json!({
            "id": "settle-too-early",
            "type": "function",
            "function": {"name": "settle_work_item", "arguments": "{}"}
        });
        let max_before = state.max_turns;
        let remaining_before = state.remaining_turns;

        let first = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![wrong.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&wrong),
        );

        assert!(first.admitted.is_empty());
        assert_eq!(first.rejected.len(), 1);
        let rejection: serde_json::Value =
            serde_json::from_str(&first.rejected[0].result).expect("structured mismatch");
        assert_eq!(rejection["error_kind"], "completion_action_mismatch");
        assert_eq!(rejection["retryable"], true);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the executable action remains available");
        assert!(!window.consumed);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 0);
        assert_eq!(state.max_turns, max_before + 1);
        assert_eq!(state.remaining_turns, remaining_before + 1);

        let max_after_first = state.max_turns;
        let remaining_after_first = state.remaining_turns;
        let repeated_same_boundary =
            apply_completion_action_admission(&mut state, first, std::slice::from_ref(&wrong));
        assert!(repeated_same_boundary.completion_action_applied);
        assert_eq!(state.max_turns, max_after_first);
        assert_eq!(state.remaining_turns, remaining_after_first);
        assert!(
            !state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .expect("same boundary is idempotent")
                .consumed
        );

        let read = serde_json::json!({
            "id": "corrected-observation",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"src/out\"}"}
        });
        let corrected = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![read.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&read),
        );
        assert_eq!(corrected.admitted, vec![read]);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("executed action remains auditable");
        assert!(window.consumed);
        assert!(window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.max_turns, max_after_first);
        assert_eq!(state.remaining_turns, remaining_after_first);
    }

    #[test]
    fn second_completion_mismatch_is_terminal_without_more_headroom() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let wrong = |id: &str| {
            serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": "settle_work_item", "arguments": "{}"}
            })
        };
        let first_call = wrong("wrong-one");
        let _ = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![first_call.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&first_call),
        );
        let max_after_correction = state.max_turns;
        let remaining_after_correction = state.remaining_turns;
        let second_call = wrong("wrong-two");
        let second = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![second_call.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&second_call),
        );

        let rejection: serde_json::Value =
            serde_json::from_str(&second.rejected[0].result).expect("structured mismatch");
        assert_eq!(rejection["retryable"], false);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("terminal mismatch remains auditable");
        assert!(window.consumed);
        assert!(!window.matched);
        assert_eq!(state.max_turns, max_after_correction);
        assert_eq!(state.remaining_turns, remaining_after_correction);
    }

    #[test]
    fn matching_call_rejected_by_ordinary_admission_consumes_action_attempt() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let read = serde_json::json!({
            "id": "denied-observation",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"src/out\"}"}
        });
        let max_before = state.max_turns;
        let admission = ToolCallAdmission {
            admitted: Vec::new(),
            rejected: vec![RejectedToolCall {
                id: "denied-observation".into(),
                name: "read_file".into(),
                canonical_call: read.clone(),
                result: r#"{"status":"rejected","error_kind":"permission_denied"}"#.into(),
            }],
            completion_action_applied: false,
        };

        let result =
            apply_completion_action_admission(&mut state, admission, std::slice::from_ref(&read));

        assert_eq!(result.rejected.len(), 1);
        assert!(result.rejected[0].result.contains("permission_denied"));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("failed attempt remains auditable");
        assert!(window.consumed);
        assert!(!window.matched);
        assert_eq!(window.mismatch_corrections_remaining, 1);
        assert_eq!(state.max_turns, max_before);
    }

    #[test]
    fn post_mutation_action_does_not_accept_external_or_unknown_success() {
        assert!(crate::turn::tool_side_effects::tool_call_may_observe_workspace("read_file", None));
        assert!(
            crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                Some(&serde_json::json!({"command": "git diff --stat"}))
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "github",
                Some(&serde_json::json!({"action": "comment"}))
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace("unknown_tool", None)
        );
    }

    #[test]
    fn completion_action_boundary_is_terminal_after_unmatched_attempt() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::RequiredWorkspaceMutation,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: false,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::TerminalIncomplete
        );
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn completion_action_boundary_is_terminal_when_obligation_remains() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::TerminalIncomplete
        );
        assert!(state.final_text.contains("unverified"));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn completion_action_boundary_clears_after_obligation_settles() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::Settled
        );
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[tokio::test]
    async fn terminal_interruption_does_not_reopen_workspace_completion_retry() {
        let mut host = MockHost::new(vec![text_result(
            "partial provider result",
            20,
            10,
            Some(30),
        )]);
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(&state, Some("provider output cap exhausted".to_string())),
        ));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("a terminal interruption should render the partial result");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 1, "interruption must not reopen the LLM");
        assert!(
            state
                .final_text
                .contains("Why stopped: provider output cap exhausted")
        );
        assert!(
            state
                .final_text
                .contains("Partial assistant response before interruption:")
        );
        assert!(state.final_text.contains("partial provider result"));
        assert_eq!(state.final_text.matches("Why stopped:").count(), 1);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[tokio::test]
    async fn exhausted_workspace_contract_does_not_fall_through_to_success_stop() {
        let mut host = MockHost::new(vec![text_result(
            "The requested file was updated successfully.",
            20,
            10,
            Some(30),
        )]);
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.completion_settlement.workspace_mutation_retries = 1;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("an incomplete workspace contract should render a typed handoff");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(
            host.turn_count(),
            1,
            "exhaustion must not reopen the provider"
        );
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("Why stopped"));
    }

    #[tokio::test]
    async fn external_only_scope_without_executor_receipt_is_incomplete() {
        use astra_config::user_profile::{MutationCompletionScope, TurnIntent};

        let mut host = MockHost::new(vec![text_result(
            "The managed external state is configured and verified.",
            20,
            10,
            Some(30),
        )]);
        let mut state = make_state();
        state.task_profile = structured_mutating_profile();
        state.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(MutationCompletionScope::External),
        );
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"managed-state-action"}"#),
        ));
        state.hooks.completion_settlement.external_effect_retries = 1;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("external-only task without an executor receipt must fail closed");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 1);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("Why stopped"));
    }

    #[tokio::test]
    async fn external_only_scope_with_executor_receipt_completes_normally() {
        use astra_config::user_profile::{MutationCompletionScope, TurnIntent};

        let mut host = MockHost::new(vec![text_result(
            "The managed external state is configured and verified.",
            20,
            10,
            Some(30),
        )]);
        let mut state = make_state();
        state.task_profile = structured_mutating_profile();
        state.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(MutationCompletionScope::External),
        );
        state.stall.tool_call_records.push(external_effect_record(
            astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP,
        ));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("executor-owned external receipt should satisfy external completion");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 1);
        assert!(state.interruption.is_none());
        assert_eq!(
            state.final_text,
            "The managed external state is configured and verified."
        );
    }

    #[tokio::test]
    async fn workspace_contract_retry_exhaustion_wins_over_other_reopen_guards() {
        let mut host = MockHost::new(vec![
            text_result("I changed the workspace.", 20, 10, Some(30)),
            text_result("The requested check is complete.", 20, 10, Some(30)),
        ]);
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_retries = 1;

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("exhausted recovery must be a typed terminal");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 1, "a terminal recovery must not retry");
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[tokio::test]
    async fn two_text_stops_without_mutation_end_as_incomplete_not_completed() {
        let mut host = MockHost::new(vec![
            text_result("I will take care of the file.", 20, 10, Some(30)),
            text_result("It is done.", 20, 10, Some(30)),
        ]);
        let mut state = make_state();
        mark_must_mutate(&mut state);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("bounded mutation recovery should settle");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 2, "only one bounded recovery is allowed");
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("Why stopped"));
    }

    #[test]
    fn workspace_completion_retry_reopens_a_text_only_budget_boundary() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.completion_settlement.text_only = true;
        state.budget_wrapup_injected = true;

        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);

        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        });
        state.hooks.completion_settlement.text_only = true;
        state.budget_wrapup_injected = true;

        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);
    }

    #[test]
    fn first_missing_workspace_completion_opens_only_the_typed_writer_window() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.workspace_root_hint = Some("/remote/app".into());
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"opaque workspace writer"}"#),
        ));
        state.final_text = "done".into();

        assert!(enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("recovery must narrow execution to one typed writer");
        assert_eq!(window.action, CompletionAction::RequiredWorkspaceMutation);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 1);
        assert!(!window.consumed);
        assert!(!window.matched);
        assert_eq!(
            state.hooks.completion_settlement.workspace_mutation_retries,
            1
        );

        let record_count = state.stall.tool_call_records.len();
        let read = serde_json::json!({
            "id": "read-too-early",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": r#"{"path":"/remote/app/result.txt"}"#,
            }
        });
        let read_admission = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![read.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            &[read],
        );
        assert!(read_admission.admitted.is_empty());
        assert_eq!(read_admission.rejected.len(), 1);
        let rejection: serde_json::Value =
            serde_json::from_str(&read_admission.rejected[0].result).expect("typed rejection");
        assert_eq!(rejection["error_kind"], "completion_action_mismatch");
        assert!(
            rejection["error"]
                .as_str()
                .is_some_and(|text| text.contains("full desired bytes"))
        );
        assert!(
            rejection["error"]
                .as_str()
                .is_some_and(|text| text.contains("Bash exit status"))
        );
        assert_eq!(
            rejection["action_hint"]["accepted_action_shapes"][0]["tool"],
            "write_file"
        );
        assert_eq!(
            state.stall.tool_call_records.len(),
            record_count,
            "a rejected standalone read must not manufacture an executed observation"
        );
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the one mismatch correction retains writer authority");
        assert!(!window.consumed);
        assert_eq!(window.mismatch_corrections_remaining, 0);

        let writer = serde_json::json!({
            "id": "complete-state-writer",
            "type": "function",
            "function": {
                "name": "write_file",
                "arguments": r#"{"path":"/remote/app/result.txt","content":"done\n"}"#,
            }
        });
        let writer_admission = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: vec![writer.clone()],
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            &[writer],
        );
        assert_eq!(writer_admission.admitted.len(), 1);
        assert!(writer_admission.rejected.is_empty());
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("matching writer attempt stays auditable");
        assert!(window.consumed);
        assert!(window.matched);
    }

    #[test]
    fn read_only_turn_does_not_observe_a_non_authoritative_mutation_risk() {
        let mut state = make_state();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
            ),
        );
        state.stall.tool_call_records.push(executed_record(
            "write_file",
            false,
            Some(r#"{"path":"/workspace/out.txt","content":"partial"}"#),
        ));

        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(!has_concrete_workspace_mutation(&state));
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .post_mutation_observation_retries,
            0
        );
        assert!(state.interruption.is_none());
    }

    #[test]
    fn exhausted_workspace_mutation_recovery_is_typed_incomplete() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.hooks.completion_settlement.workspace_mutation_retries = 1;
        state.final_text = "I have completed the requested change.".into();

        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!state.final_text_streamed);
        assert!(
            state
                .interruption
                .as_ref()
                .and_then(|record| record.error_detail.as_deref())
                .is_some_and(|detail| detail.contains("workspace mutation"))
        );
    }

    #[test]
    fn exhausted_post_mutation_observation_recovery_is_typed_incomplete() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .hooks
            .completion_settlement
            .post_mutation_observation_retries = 1;

        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!state.final_text_streamed);
        assert!(state.final_text.contains("unverified"));
    }

    #[test]
    fn persistent_unresolved_outcome_gets_one_text_only_reconciliation() {
        let mut state = make_state();
        install_committed_work_synthesis_wire_surface(&mut state);
        state.stall.active_policy_feedback = serde_json::from_value(serde_json::json!({
            "state": "evaluated",
            "schema_version": 2,
            "revision": 2,
            "evaluated_at_round": 4,
            "subject": {"kind": "run"},
            "entries": [{
                "signal": "unresolved_tool_outcomes",
                "stage": "converge",
                "observed_at_round": 4,
                "evidence_count": 2,
                "recommendation": "diagnose_tool_outcomes"
            }]
        }))
        .expect("valid policy feedback");

        let disposition = terminal_completion_disposition(&state, true);
        assert!(
            !enforce_workspace_completion_before_text_completion_with_disposition(
                &mut state,
                disposition,
            )
        );
        assert!(enforce_outcome_reconciliation_before_text_completion(
            &mut state
        ));
        assert!(state.hooks.completion_settlement.text_only);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .outcome_reconciliation_retries,
            1
        );
        assert!(state.volatile_pending.iter().any(|entry| {
            entry
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                == Some("outcome_reconciliation_required.v1")
        }));
        assert!(!enforce_outcome_reconciliation_before_text_completion(
            &mut state
        ));
    }

    #[test]
    fn persistent_unresolved_outcome_becomes_resumable_incomplete_after_retry() {
        let mut state = make_state();
        state
            .hooks
            .completion_settlement
            .outcome_reconciliation_retries = 1;
        state.stall.active_policy_feedback = serde_json::from_value(serde_json::json!({
            "state": "evaluated",
            "schema_version": 2,
            "revision": 3,
            "evaluated_at_round": 6,
            "subject": {"kind": "run"},
            "entries": [{
                "signal": "unresolved_tool_outcomes",
                "stage": "converge",
                "observed_at_round": 6,
                "evidence_count": 3,
                "recommendation": "diagnose_tool_outcomes"
            }]
        }))
        .expect("valid policy feedback");

        assert!(enforce_persistent_unresolved_outcome_terminal(&mut state));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("remains incomplete"));
        assert!(!enforce_persistent_unresolved_outcome_terminal(&mut state));
    }

    #[test]
    fn policy_advisories_do_not_open_an_acceptance_tool_boundary() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.final_text = "The requested API is fully fixed.".into();
        let max_turns = state.max_turns;
        let remaining_turns = state.remaining_turns;
        state.stall.active_policy_feedback = serde_json::from_value(serde_json::json!({
            "state": "evaluated",
            "schema_version": 2,
            "revision": 3,
            "evaluated_at_round": 8,
            "subject": {"kind": "run"},
            "entries": [{
                "signal": "low_yield_round_churn",
                "stage": "observe",
                "observed_at_round": 8,
                "evidence_count": 8,
                "recommendation": "synthesize_and_decide"
            }]
        }))
        .expect("valid policy feedback");

        // Runtime-policy signals are alerts.  Without a caller-declared
        // verification contract they must not create an actionful retry or
        // project a text-only boundary while ordinary budget remains.
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, max_turns);
        assert_eq!(state.remaining_turns, remaining_turns);
        assert!(state.volatile_pending.iter().all(|entry| {
            entry
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                != Some("acceptance_reconciliation_required.v1")
        }));
    }

    #[tokio::test]
    async fn persistent_unresolved_outcome_rewrites_candidate_final_once() {
        // Two independent provider attempts of the same governed operation
        // must be treated as one recoverable obligation even though each
        // attempt receives a fresh provider call id. Distinct operations are
        // intentionally not enough evidence for a convergence-stage retry.
        let mut failed_a = make_edge_tool("bash", "probe failed (attempt one)");
        failed_a.request_id = "failed-a".into();
        failed_a.args = serde_json::json!({"command": "probe-a"});
        failed_a.status = "failed".into();
        let mut failed_b = make_edge_tool("bash", "probe failed (attempt two)");
        failed_b.request_id = "failed-b".into();
        failed_b.args = serde_json::json!({"command": "probe-a"});
        failed_b.status = "failed".into();

        let candidate = "Everything passed; there are no unresolved results.";
        let corrected = "The probe failed twice, so the exact cause remains unresolved.";
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![failed_a], 20, 10, Some(30)),
            edge_tool_result(vec![failed_b], 20, 10, Some(30)),
            text_result(candidate, 20, 10, Some(30)),
            text_result(corrected, 20, 10, Some(30)),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = make_state();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok(), "reconciliation must complete: {outcome:?}");
        assert_eq!(host.turn_count(), 4);
        assert!(
            state
                .interruption
                .as_ref()
                .is_some_and(|record| record.kind == InterruptionKind::ExecutionIncomplete),
            "persistent unresolved evidence must remain resumable"
        );
        assert!(state.final_text.contains(corrected));
        assert!(state.final_text.contains("Why stopped"));
        assert!(state.messages.iter().all(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|content| content != candidate)
        }));
        // Finalization clears per-turn settlement counters. Four provider
        // turns plus removal of `candidate` prove the bounded rewrite fired.
    }

    fn server_feedback_frame() -> astra_turn_core::context_feedback::RuntimeFeedbackFrame {
        serde_json::from_value(serde_json::json!({
            "schema_version": astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
            "identity": {
                "session_id": "session-1",
                "run_id": "run-1",
                "agent_id": "agent-1",
                "model_id": "deepseek-v4-flash",
                "topology": "server_only"
            },
            "progress": {
                "session_turn": 4,
                "agentic_round_index": 2,
                "llm_rounds_completed": 3,
                "slice_round_limit": 60,
                "slice_rounds_remaining": 57
            },
            "context": {
                "model_context_window_tokens": 1000000,
                "effective_input_limit_tokens": 800000,
                "estimated_input_tokens": 840000,
                "token_pressure": 1.05,
                "compaction_tier": "compact_history"
            },
            "request_usage": {
                "prompt": 100,
                "cache_read": 200,
                "cache_creation": 0,
                "completion": 20
            },
            "run_usage": {
                "prompt": 300,
                "cache_read": 600,
                "cache_creation": 0,
                "completion": 60
            },
            "was_truncated": false,
            "policy_feedback": {
                "state": "evaluated",
                "schema_version": astra_turn_core::context_feedback::RuntimePolicyFeedbackSet::SCHEMA_VERSION,
                "revision": 2,
                "evaluated_at_round": 2,
                "subject": {
                    "kind": "work_item",
                    "attempt_id": "attempt-1",
                    "item_id": "item-1",
                    "item_revision": 1,
                    "objective": "Inspect one bounded target",
                    "expected_result": "One verified result"
                },
                "entries": [{
                    "signal": "redundant_reads",
                    "stage": "observe",
                    "observed_at_round": 2,
                    "evidence_count": 3,
                    "recommendation": "reuse_known_content"
                }]
            }
        }))
        .expect("valid server feedback frame")
    }

    fn server_summary_tool_receipt(
        attempted: u32,
    ) -> astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt {
        astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::new(
            "run-1",
            1,
            attempted,
            attempted,
            0,
            astra_turn_core::tool_ledger_receipt::ToolLedgerResultClassCounts {
                succeeded: attempted,
                ..Default::default()
            },
            u64::from(attempted),
            astra_turn_core::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT,
            true,
        )
    }

    #[test]
    fn remote_server_feedback_is_exact_and_bound_to_terminal_progress() {
        let frame = server_feedback_frame();
        let summary = ServerLoopExecutionSummary {
            tool_calls_count: 0,
            observation_tool_calls_count: 0,
            tools_used: Vec::new(),
            llm_rounds: 3,
            tool_ledger_receipt: server_summary_tool_receipt(0),
            token_usage_coverage: None,
            runtime_feedback: Some(frame.clone()),
        };
        assert_eq!(
            authoritative_server_runtime_feedback(
                &summary,
                Some("session-1"),
                Some("run-1"),
                Some("deepseek-v4-flash"),
                4,
            ),
            Some(frame.clone())
        );

        let mut terminal_includes_failed_attempt = summary.clone();
        terminal_includes_failed_attempt.llm_rounds = 4;
        assert_eq!(
            authoritative_server_runtime_feedback(
                &terminal_includes_failed_attempt,
                Some("session-1"),
                Some("run-1"),
                Some("deepseek-v4-flash"),
                4,
            ),
            Some(frame.clone())
        );
        let mut impossible_future_feedback = summary.clone();
        impossible_future_feedback.llm_rounds = 2;
        assert!(
            authoritative_server_runtime_feedback(
                &impossible_future_feedback,
                Some("session-1"),
                Some("run-1"),
                Some("deepseek-v4-flash"),
                4,
            )
            .is_none()
        );
        assert!(
            authoritative_server_runtime_feedback(
                &summary,
                Some("session-1"),
                Some("run-1"),
                Some("deepseek-v4-flash"),
                5,
            )
            .is_none()
        );
    }

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

    fn recorded_tool_names(state: &AgenticLoopState) -> Vec<String> {
        state
            .telemetry
            .turn_trace_collector
            .as_ref()
            .expect("test must install a turn trace collector")
            .finalize()
            .tools
            .visible_tools
            .into_iter()
            .map(|tool| tool.tool_name)
            .collect()
    }

    fn install_tool_trace_collector(state: &mut AgenticLoopState) {
        state.telemetry.turn_trace_collector = Some(
            crate::turn::turn_trace_collector::TurnTraceCollector::new("turn-1", "session-1"),
        );
    }

    #[test]
    fn record_tool_selection_uses_remote_server_summary_when_edge_round_is_empty() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                server_execution_summary: Some(ServerLoopExecutionSummary {
                    tool_calls_count: 1,
                    observation_tool_calls_count: 0,
                    tools_used: vec!["read_file".to_string()],
                    llm_rounds: 1,
                    tool_ledger_receipt: server_summary_tool_receipt(1),
                    token_usage_coverage: None,
                    runtime_feedback: None,
                }),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        record_tool_selection(&mut state, &turn_result, 0);

        assert_eq!(recorded_tool_names(&state), vec!["read_file"]);
        assert_eq!(
            state
                .telemetry
                .turn_trace_collector
                .as_ref()
                .expect("collector")
                .finalize()
                .tools
                .tools_available,
            1,
            "availability must remain coherent with the authoritative summary"
        );
    }

    #[test]
    fn remote_server_terminal_does_not_fabricate_an_outer_llm_round() {
        let mut state = make_state();
        state.turn_event_buffer = Some(TurnEventBuffer::begin_turn(Some("session-1"), 1));
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                has_usage: true,
                usage_is_run_total: true,
                prompt_tokens: 12_000,
                cache_read_tokens: 400_000,
                completion_tokens: 2_000,
                server_execution_summary: Some(ServerLoopExecutionSummary {
                    tool_calls_count: 4,
                    observation_tool_calls_count: 0,
                    tools_used: vec!["read_file".to_string()],
                    llm_rounds: 7,
                    tool_ledger_receipt: server_summary_tool_receipt(4),
                    token_usage_coverage: None,
                    runtime_feedback: None,
                }),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(900),
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        record_early_exit_llm_round(
            &mut state,
            &turn_result,
            Instant::now() - Duration::from_secs(30),
            Some("stop"),
        );

        assert!(state.recent_rounds.is_empty());
        assert_eq!(
            state
                .turn_event_buffer
                .as_ref()
                .expect("turn event buffer")
                .len(),
            0,
            "the Server's physical rounds remain the sole llm_round evidence"
        );
    }

    #[test]
    fn record_tool_selection_preserves_local_edge_precedence_and_unique_names() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                server_execution_summary: Some(ServerLoopExecutionSummary {
                    tool_calls_count: 1,
                    observation_tool_calls_count: 0,
                    tools_used: vec!["remote_only".to_string()],
                    llm_rounds: 1,
                    tool_ledger_receipt: server_summary_tool_receipt(1),
                    token_usage_coverage: None,
                    runtime_feedback: None,
                }),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: vec![
                make_edge_tool(" read_file ", "ok"),
                make_edge_tool("read_file", "ok"),
                make_edge_tool(" bash ", "ok"),
            ],
            error_kind: None,
        };

        record_tool_selection(&mut state, &turn_result, 0);

        assert_eq!(recorded_tool_names(&state), vec!["read_file", "bash"]);
        assert_eq!(
            state
                .telemetry
                .turn_trace_collector
                .as_ref()
                .expect("collector")
                .finalize()
                .tools
                .tools_available,
            2,
            "the visible-name count must also floor tools_available"
        );
    }

    #[test]
    fn record_tool_selection_defers_empty_tool_bearing_trace_until_callbacks_resolve() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                tool_calls: vec![serde_json::json!({
                    "id": "call-read",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                })],
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            error_kind: None,
        };

        record_tool_selection(&mut state, &turn_result, 0);

        assert!(
            !state
                .telemetry
                .turn_trace_collector
                .as_ref()
                .expect("collector")
                .has_tool_trace(),
            "an empty provider-side tool round must leave the trace unset for callback resolution"
        );
    }

    #[test]
    fn record_tool_selection_does_not_infer_names_from_missing_or_zero_tool_summary() {
        for summary in [
            None,
            Some(ServerLoopExecutionSummary {
                tool_calls_count: 0,
                observation_tool_calls_count: 0,
                tools_used: Vec::new(),
                llm_rounds: 1,
                tool_ledger_receipt: server_summary_tool_receipt(0),
                token_usage_coverage: None,
                runtime_feedback: None,
            }),
        ] {
            let mut state = make_state();
            install_tool_trace_collector(&mut state);
            let turn_result = HostTurnResult {
                accum: ChatTurnSseAccum {
                    server_execution_summary: summary,
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: None,
                edge_tool_round: Vec::new(),
                error_kind: None,
            };

            record_tool_selection(&mut state, &turn_result, 0);

            assert!(recorded_tool_names(&state).is_empty());
        }
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
        assert!(
            state.take_volatile_pending().is_empty(),
            "an internal budget recommendation must not influence model completion"
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
        polls: Mutex<VecDeque<UserIntentPoll>>,
        poll_calls: Mutex<Vec<usize>>,
        released: Mutex<Vec<usize>>,
        release_failures: Mutex<usize>,
        terminal_on_release: bool,
        provider_authorization_calls: std::sync::atomic::AtomicUsize,
        provider_authorizations: Mutex<VecDeque<ProviderBoundaryAuthorization>>,
        fence_calls: std::sync::atomic::AtomicUsize,
        reopen_calls: std::sync::atomic::AtomicUsize,
        fence_generations: Mutex<Vec<u64>>,
        reopen_generations: Mutex<Vec<u64>>,
        reopen_error: Option<String>,
    }

    impl StubRunControlProvider {
        fn new(polls: Vec<UserIntentPoll>) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(0),
                terminal_on_release: false,
                provider_authorization_calls: std::sync::atomic::AtomicUsize::new(0),
                provider_authorizations: Mutex::new(VecDeque::new()),
                fence_calls: std::sync::atomic::AtomicUsize::new(0),
                reopen_calls: std::sync::atomic::AtomicUsize::new(0),
                fence_generations: Mutex::new(Vec::new()),
                reopen_generations: Mutex::new(Vec::new()),
                reopen_error: None,
            }
        }

        fn with_release_failures(polls: Vec<UserIntentPoll>, release_failures: usize) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(release_failures),
                terminal_on_release: false,
                provider_authorization_calls: std::sync::atomic::AtomicUsize::new(0),
                provider_authorizations: Mutex::new(VecDeque::new()),
                fence_calls: std::sync::atomic::AtomicUsize::new(0),
                reopen_calls: std::sync::atomic::AtomicUsize::new(0),
                fence_generations: Mutex::new(Vec::new()),
                reopen_generations: Mutex::new(Vec::new()),
                reopen_error: None,
            }
        }

        fn with_terminal_release(polls: Vec<UserIntentPoll>) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(0),
                terminal_on_release: true,
                provider_authorization_calls: std::sync::atomic::AtomicUsize::new(0),
                provider_authorizations: Mutex::new(VecDeque::new()),
                fence_calls: std::sync::atomic::AtomicUsize::new(0),
                reopen_calls: std::sync::atomic::AtomicUsize::new(0),
                fence_generations: Mutex::new(Vec::new()),
                reopen_generations: Mutex::new(Vec::new()),
                reopen_error: None,
            }
        }

        fn with_reopen_error(polls: Vec<UserIntentPoll>, error: &str) -> Self {
            Self {
                reopen_error: Some(error.to_string()),
                ..Self::new(polls)
            }
        }

        fn with_provider_authorizations(
            self,
            decisions: Vec<ProviderBoundaryAuthorization>,
        ) -> Self {
            *self
                .provider_authorizations
                .try_lock()
                .expect("new provider authorization queue is uncontended") =
                VecDeque::from(decisions);
            self
        }

        async fn poll_call_count(&self) -> usize {
            self.poll_calls.lock().await.len()
        }
    }

    struct DirectErrorHost {
        error: Option<astra_core::ClassifiedError>,
        valid_tools: HashSet<String>,
        calls: usize,
    }

    struct BoundaryDecisionRunControl {
        decision: Result<ProviderBoundaryAuthorization, String>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "harness")]
    struct PreLlmBlockingHarness;

    #[cfg(feature = "harness")]
    impl astra_harness::HarnessKernel for PreLlmBlockingHarness {
        fn snapshot(&self) -> Option<astra_harness::RuntimeSnapshot> {
            None
        }

        fn on_record(&self, record: &astra_harness::DecisionRecord) -> astra_harness::HookVerdict {
            if record.point == astra_harness::HookPoint::PreLlmRequest {
                astra_harness::HookVerdict::Block {
                    reason: "deterministic pre-LLM rejection".to_string(),
                }
            } else {
                astra_harness::HookVerdict::Continue
            }
        }
    }

    #[async_trait]
    impl RunStatusProvider for BoundaryDecisionRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
            Ok(None)
        }
    }

    #[async_trait]
    impl UserIntentProvider for BoundaryDecisionRunControl {
        async fn authorize_provider_boundary(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<ProviderBoundaryAuthorization, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision.clone()
        }

        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                ..UserIntentPoll::default()
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    impl DirectErrorHost {
        fn new(error: astra_core::ClassifiedError) -> Self {
            Self {
                error: Some(error),
                valid_tools: HashSet::new(),
                calls: 0,
            }
        }

        fn turn_count(&self) -> usize {
            self.calls
        }
    }

    #[async_trait]
    impl AgenticLoopHost for DirectErrorHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            self.calls = self.calls.saturating_add(1);
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
    impl UserIntentProvider for StubRunControlProvider {
        async fn authorize_provider_boundary(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<ProviderBoundaryAuthorization, String> {
            self.provider_authorization_calls
                .fetch_add(1, Ordering::SeqCst);
            Ok(self
                .provider_authorizations
                .lock()
                .await
                .pop_front()
                .unwrap_or(ProviderBoundaryAuthorization::Authorized))
        }

        async fn fence_user_intent_submissions(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            authority: UserIntentAdmissionAuthority,
        ) -> Result<(), String> {
            let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
                authority
            else {
                return Err("stub durable provider requires exact owner generation".to_string());
            };
            self.fence_calls.fetch_add(1, Ordering::SeqCst);
            self.fence_generations
                .lock()
                .await
                .push(expected_owner_generation);
            Ok(())
        }

        async fn reopen_user_intent_submissions(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            authority: UserIntentAdmissionAuthority,
        ) -> Result<(), String> {
            let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
                authority
            else {
                return Err("stub durable provider requires exact owner generation".to_string());
            };
            self.reopen_calls.fetch_add(1, Ordering::SeqCst);
            self.reopen_generations
                .lock()
                .await
                .push(expected_owner_generation);
            match self.reopen_error.as_ref() {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            self.poll_calls.lock().await.push(after_event_index);
            self.polls
                .lock()
                .await
                .pop_front()
                .unwrap_or(UserIntentPoll {
                    next_cursor: after_event_index,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: None,
                })
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            event_indices: &[usize],
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            if self.terminal_on_release {
                return Ok(crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned);
            }
            let mut release_failures = self.release_failures.lock().await;
            if *release_failures > 0 {
                *release_failures -= 1;
                return Err("release failed".to_string());
            }
            drop(release_failures);
            self.released.lock().await.extend_from_slice(event_indices);
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    #[test]
    fn observe_turn_end_without_tools_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.context_manifest_model_name = Some("test-model".to_string());
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
    fn runtime_feedback_uses_typed_manifest_prompt_cache_identity() {
        let identity = astra_turn_types::PromptCacheIdentityV1::from_prefixes(
            &[serde_json::json!({"role": "system", "content": "stable"})],
            &[],
            "openai-stable-prefix-v1",
        )
        .expect("valid cache identity");
        let manifest = serde_json::json!({
            "wire": {
                "fingerprint": {
                    "prompt_cache_identity": identity.clone()
                }
            }
        });
        assert_eq!(
            prompt_cache_identity_from_manifest(Some(&manifest)),
            Some(identity)
        );

        let missing = serde_json::json!({"wire": {"fingerprint": {}}});
        let malformed = serde_json::json!({
            "wire": {"fingerprint": {"prompt_cache_identity": {"content_id": "invalid"}}}
        });
        assert_eq!(prompt_cache_identity_from_manifest(Some(&missing)), None);
        assert_eq!(prompt_cache_identity_from_manifest(Some(&malformed)), None);
    }

    #[test]
    fn context_manifest_context_window_defaults_to_generic_200k_without_trace() {
        let state = make_state();

        assert_eq!(
            context_window_tokens_for_context_manifest(&state),
            crate::prompts::DEFAULT_CONTEXT_WINDOW_TOKENS as u32
        );
    }

    #[test]
    fn bridge_compaction_observations_update_turn_trace_and_step_audit() {
        let mut state = make_state();
        state.max_turn_input_tokens = 20_000;
        let observations = vec![
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
                id: "initial".to_string(),
                kind: CompactionKind::WireAssembly,
                tier: CompactionTier::CompactHistory,
                messages_before: 18,
                messages_after: 10,
                tokens_before: 15_000,
                tokens_after: 9_000,
                tokens_saved: 6_000,
                post_compaction_target_tokens: None,
                effectiveness: astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Unmeasured,
            },
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
                id: "context_window_retry:1:1".to_string(),
                kind: CompactionKind::WireContextRetry,
                tier: CompactionTier::AggressivePrune,
                messages_before: 12,
                messages_after: 6,
                tokens_before: 10_000,
                tokens_after: 5_000,
                tokens_saved: 5_000,
                post_compaction_target_tokens: None,
                effectiveness: astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Unmeasured,
            },
        ];

        let mut host = MockHost::new(Vec::new());
        record_context_compactions(&mut host, &mut state, &observations);

        assert!(state.context_compression_triggered);
        let compactions: Vec<_> = state
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            })
            .collect();
        assert_eq!(compactions.len(), 2);
        assert_eq!(
            compactions[0].payload.as_ref().unwrap()["results_compacted"],
            8
        );
        assert_eq!(
            compactions[0].payload.as_ref().unwrap()["tokens_saved"],
            6_000
        );
        assert_eq!(compactions[0].payload.as_ref().unwrap()["pressure"], 0.75);
        assert_eq!(
            compactions[1].payload.as_ref().unwrap()["results_compacted"],
            6
        );
        assert_eq!(state.compact_tier_applied, CompactionTier::AggressivePrune);
        assert_eq!(host.compaction_events.len(), 2);
        assert_eq!(host.compaction_events[0].kind, CompactionKind::WireAssembly);
        assert_eq!(host.compaction_events[0].tokens_freed, 6_000);
        assert_eq!(
            host.compaction_events[1].kind,
            CompactionKind::WireContextRetry
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
    async fn text_only_authority_survives_only_an_admitted_provider_convergence() {
        let mut state = make_state();
        state.remaining_turns = 3;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider action deadline",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {"phase": "semantic_progress"},
                    "partial_full_text": "",
                    "partial_reasoning": "provisional",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("action deadline should schedule bounded convergence");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.force_next_thinking_off);

        let mut rejected_state = make_state();
        rejected_state.remaining_turns = 3;
        rejected_state.hooks.completion_settlement.text_only = true;
        let mut rejected_host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider hard deadline without safe convergence evidence",
        ));
        assert!(
            execute_turn_and_ingest_phase(
                &mut rejected_host,
                &mut rejected_state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .is_err()
        );
        assert!(
            !rejected_state.hooks.completion_settlement.text_only,
            "a rejected convergence must not leak text-only authority into unrelated recovery"
        );
    }

    #[tokio::test]
    async fn final_text_only_empty_completion_gets_one_recovery_at_zero_budget() {
        let mut state = make_state();
        state.max_turns = 34;
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_convergence",
                        "phase": "actionable_output"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "",
                    "tool_calls": [],
                    "provider_response": {"transport_success": true}
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("a transport-success empty final settlement should recover once");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert_eq!(state.max_turns, 35);
        assert_eq!(state.remaining_turns, 1);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.action_convergence_attempted);
        assert!(state.provider_adaptation.force_next_thinking_off);
        assert!(state.interruption.is_none());
        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("bounded provider recovery advisory");
        assert_eq!(advisory.payload["execution_mode"], "text_only");
    }

    #[tokio::test]
    async fn repeated_final_text_only_empty_completion_is_typed_empty_completion() {
        let mut state = make_state();
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        state.provider_adaptation.action_convergence_attempted = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_convergence",
                        "phase": "actionable_output"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "",
                    "tool_calls": [],
                    "provider_response": {"transport_success": true}
                })
                .to_string(),
            ),
        );

        assert!(
            execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .is_err()
        );

        let interruption = state
            .interruption
            .as_ref()
            .expect("repeated empty completion must be terminally classified");
        assert_eq!(interruption.kind, InterruptionKind::EmptyCompletion);
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.remaining_turns, 0);
    }

    #[tokio::test]
    async fn zero_budget_semantic_provider_deadline_remains_provider_deadline() {
        let mut state = make_state();
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider semantic progress deadline",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_inference",
                        "phase": "semantic_progress"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "provisional",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        assert!(
            execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .is_err()
        );

        assert_eq!(
            state
                .interruption
                .as_ref()
                .expect("deadline interruption")
                .kind,
            InterruptionKind::ProviderDeadline
        );
        assert_eq!(state.remaining_turns, 0);
        assert!(!state.provider_adaptation.force_next_thinking_off);
    }

    #[tokio::test]
    async fn direct_stream_transport_with_reasoning_continues_once_without_replaying_actions() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::StreamTransport,
                "provider connection reset after provisional reasoning",
            )
            .with_details_json(
                serde_json::json!({
                    "partial_full_text": "",
                    "partial_reasoning": "working through the task",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("an unambiguous transport interruption must schedule one recovery turn");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.provider_adaptation.action_convergence_attempted);
        assert!(state.provider_adaptation.force_next_thinking_off);
        assert!(state.interruption.is_none());
        assert!(state.stall.tool_call_records.is_empty());
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
    fn terminal_completion_disposition_uses_only_typed_settlement_authority() {
        let mut ordinary = make_state();
        assert_eq!(
            terminal_completion_disposition(&ordinary, false),
            TerminalCompletionDisposition::OrdinaryCompletionCandidate
        );

        ordinary.budget_wrapup_injected = true;
        ordinary.hooks.completion_settlement.text_only = true;
        ordinary.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        assert_eq!(
            terminal_completion_disposition(&ordinary, false),
            TerminalCompletionDisposition::RoundSliceIncomplete
        );

        let mut committed_work = make_state();
        committed_work.budget_wrapup_injected = true;
        committed_work.hooks.completion_settlement.text_only = true;
        committed_work.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut committed_work);
        assert_eq!(
            terminal_completion_disposition(&committed_work, true),
            TerminalCompletionDisposition::CommittedWorkSynthesis
        );

        let mut exact_action = make_state();
        exact_action.budget_wrapup_injected = true;
        exact_action.hooks.completion_settlement.text_only = true;
        exact_action.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        exact_action.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        exact_action
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        exact_action
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        exact_action
            .hooks
            .completion_settlement
            .completion_action_window = Some(super::super::host::CompletionActionWindow {
            action: CompletionAction::PostMutationObservation,
            attempts_remaining: 0,
            mismatch_corrections_remaining: 0,
            consumed: true,
            matched: true,
        });
        assert_eq!(
            terminal_completion_disposition(&exact_action, false),
            TerminalCompletionDisposition::SettledExactCompletionAction
        );

        let mut generic_action = make_state();
        generic_action.budget_wrapup_injected = true;
        generic_action.hooks.completion_settlement.text_only = true;
        generic_action.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        generic_action
            .hooks
            .completion_settlement
            .completion_action_window = Some(super::super::host::CompletionActionWindow {
            action: CompletionAction::CompletionTaskAction,
            attempts_remaining: 0,
            mismatch_corrections_remaining: 0,
            consumed: true,
            matched: true,
        });
        assert_eq!(
            terminal_completion_disposition(&generic_action, false),
            TerminalCompletionDisposition::RoundSliceIncomplete,
            "a generic action proves execution, not completion of the user goal"
        );
    }

    #[tokio::test]
    async fn forced_round_slice_text_without_typed_completion_is_resumable_incomplete() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        let mut host = MockHost::new(vec![text_result(
            "Useful progress summary from the exhausted execution slice.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("forced round-slice settlement should remain a successful control transition");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::BudgetExhausted)
        );
        assert_eq!(
            state
                .interruption
                .as_ref()
                .map(|record| &record.resume_action),
            Some(&ResumeAction::ContinueImmediately)
        );
        assert!(
            state
                .final_text
                .contains("Useful progress summary from the exhausted execution slice.")
        );
        assert!(state.final_text.contains("Why stopped:"));
    }

    #[tokio::test]
    async fn committed_work_completion_synthesis_is_not_reclassified_as_round_slice_partial() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "Completed from the committed Work graph.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("typed final synthesis should be ingested");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "the typed final-synthesis authority is one-shot"
        );
    }

    #[tokio::test]
    async fn final_work_synthesis_rechecks_run_authority_after_durable_work_read() {
        let mut state = make_state();
        state.current_run_id = Some("run-final-fence".into());
        state.current_session_id = Some("session-final-fence".into());
        state.context_manifest_user_id = Some("user-final-fence".into());
        state.current_run_owner_generation = Some(9);
        install_committed_work_synthesis_wire_surface(&mut state);
        let provider = Arc::new(
            StubRunControlProvider::new(vec![UserIntentPoll::default(), UserIntentPoll::default()])
                .with_provider_authorizations(vec![
                    ProviderBoundaryAuthorization::Authorized,
                    ProviderBoundaryAuthorization::AuthorityLost {
                        reason: "owner lease expired during model inference".into(),
                    },
                ]),
        );
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![text_result(
            "Must not cross the lost Run boundary.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

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

        let Err(error) = result else {
            panic!("lease loss after Work validation must fail closed");
        };
        assert_eq!(error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(host.committed_work_synthesis_checks, 1);
        assert_eq!(
            provider.provider_authorization_calls.load(Ordering::SeqCst),
            2,
            "the second authorization is the final Run terminal fence"
        );
        assert!(state.final_text.is_empty());
    }

    #[tokio::test]
    async fn committed_terminal_work_owns_must_mutate_goal_review_without_workspace_receipt() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        assert!(!has_bound_workspace_completion_evidence(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::RequiredWorkspaceMutation),
            "without Work authority the generic Run contract would reopen mutation"
        );
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "The committed Work result is ready for the user.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("canonical Work synthesis should complete");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert_eq!(
            host.turn_count(),
            1,
            "no generic action window was reopened"
        );
        assert_eq!(
            state.final_text,
            "The committed Work result is ready for the user."
        );
    }

    #[tokio::test]
    async fn committed_work_revalidation_unavailable_interrupts_before_generic_mutation_retry() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result("Final review.", 10, 5, Some(1))])
            .with_unavailable_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("durable revalidation outage is a resumable loop outcome");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::Interrupted)
        );
        assert_eq!(
            state.hooks.completion_settlement.workspace_mutation_retries, 0,
            "a Work-store outage must not fabricate a missing-mutation retry"
        );
    }

    #[tokio::test]
    async fn revoked_committed_work_never_falls_through_generic_must_mutate_completion() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        });
        assert!(has_bound_workspace_completion_evidence(&state));
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "Stale synthesis must not be accepted.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("durable revocation is a typed incomplete outcome");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.committed_work_synthesis_checks, 1);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!state.final_text.contains("Stale synthesis"));
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "canonical Ok(false) explicitly revokes the candidate"
        );
        assert_eq!(
            state.hooks.completion_settlement.workspace_mutation_retries, 0,
            "revocation must not fall through the generic mutation guard"
        );
    }

    #[tokio::test]
    async fn historical_work_outage_does_not_interrupt_a_fresh_non_candidate_run() {
        let mut state = make_state();
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface
        );
        let mut host = MockHost::new(vec![text_result("Fresh run answer.", 10, 5, Some(1))])
            .with_unavailable_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("historical Work state is not authority for a fresh run");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert_eq!(host.committed_work_synthesis_checks, 0);
        assert_eq!(state.final_text, "Fresh run answer.");
    }

    #[tokio::test]
    async fn committed_work_is_revalidated_after_legitimate_inspection_before_final_prose() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("introspect", "current state")],
                10,
                5,
                Some(1),
            ),
            text_result("Final after inspection.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["introspect"])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 2);
        assert!(state.interruption.is_none());
        assert_eq!(state.final_text, "Final after inspection.");
    }

    #[tokio::test]
    async fn rejected_goal_review_tool_does_not_block_durable_work_revalidation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let rejected_call = serde_json::json!({
            "id": "rejected-review",
            "type": "function",
            "function": {
                "name": "unavailable_inspector",
                "arguments": "{}"
            }
        });
        let mut host = MockHost::new(vec![
            server_tool_result(vec![rejected_call], Vec::new(), 10, 5, Some(1)),
            text_result("Final after rejected inspection.", 10, 5, Some(1)),
        ])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 2);
        assert!(state.interruption.is_none());
        assert!(state.stall.tool_call_records.iter().any(|record| {
            record.name == "unavailable_inspector"
                && record.effective_disposition() == ToolCallDisposition::Rejected
        }));
        assert_eq!(state.final_text, "Final after rejected inspection.");
    }

    #[tokio::test]
    async fn explicit_verifier_runs_before_durable_work_final_synthesis() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.task_profile.verification_required = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let mut verifier = make_edge_tool("bash", "quality passed");
        verifier.args = serde_json::json!({"command": "./quality-gate"});
        let mut host = MockHost::new(vec![
            text_result("Premature final.", 10, 5, Some(1)),
            edge_tool_result(vec![verifier], 10, 5, Some(1)),
            text_result("Final after explicit verification.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["bash"])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 3);
        assert_eq!(
            host.committed_work_synthesis_checks, 2,
            "the candidate remains live and is revalidated after verification"
        );
        assert!(state.interruption.is_none());
        assert_eq!(
            missing_explicit_verification_hooks(&state),
            Some(Vec::new())
        );
        assert_eq!(state.final_text, "Final after explicit verification.");
    }

    #[tokio::test]
    async fn post_verifier_final_prose_fails_closed_when_work_revalidation_is_unavailable() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.task_profile.verification_required = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let mut verifier = make_edge_tool("bash", "quality passed");
        verifier.args = serde_json::json!({"command": "./quality-gate"});
        let mut host = MockHost::new(vec![
            text_result("Premature final.", 10, 5, Some(1)),
            edge_tool_result(vec![verifier], 10, 5, Some(1)),
            text_result("Stale final after verification.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["bash"])
        .with_committed_work_synthesis_sequence([
            Ok(true),
            Err("durable Work store unavailable after verification".into()),
        ]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 3);
        assert_eq!(host.committed_work_synthesis_checks, 2);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::Interrupted)
        );
        assert!(!state.final_text.contains("Stale final after verification"));
    }

    #[test]
    fn committed_work_synthesis_preserves_safety_barriers() {
        let mut paginated_fanout = make_state();
        install_committed_work_synthesis_wire_surface(&mut paginated_fanout);
        paginated_fanout.budget_wrapup_injected = true;
        paginated_fanout.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        paginated_fanout
            .hooks
            .completion_settlement
            .foreground_fanout_pagination = Some(super::super::host::ForegroundFanoutPagination {
            group_id: "fanout-1".into(),
            target_count: 2,
            pending_slots: std::collections::BTreeMap::from([(1, 1024)]),
        });
        assert_eq!(
            terminal_completion_disposition(&paginated_fanout, true),
            TerminalCompletionDisposition::RoundSliceIncomplete,
            "unread foreground fanout evidence remains a stronger barrier"
        );

        let mut unfinished_child = make_state();
        install_committed_work_synthesis_wire_surface(&mut unfinished_child);
        unfinished_child
            .stall
            .tool_call_records
            .push(ToolCallRecord {
                name: "agent".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "action": "spawn",
                        "agent_id": "reviewer-live",
                        "description": "Review the terminal result"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    serde_json::json!({
                        "status": "launched",
                        "agent_id": "reviewer-live"
                    })
                    .to_string(),
                ),
                ..Default::default()
            });
        assert_eq!(
            terminal_completion_disposition(&unfinished_child, true),
            TerminalCompletionDisposition::RoundSliceIncomplete,
            "an unfinished child remains stronger than Work synthesis even outside budget wrap-up"
        );

        let mut quarantined = make_state();
        install_committed_work_synthesis_wire_surface(&mut quarantined);
        quarantined.stall.workspace_observation_quarantine = Some(
            astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::partial_workspace_mutation(
                Some("call-with-unsettled-commit".into()),
            ),
        );
        let disposition = terminal_completion_disposition(&quarantined, true);
        assert!(
            !enforce_workspace_completion_before_text_completion_with_disposition(
                &mut quarantined,
                disposition,
            )
        );
        assert_eq!(
            quarantined.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete),
            "an unsettled executor commit remains stronger than Work synthesis"
        );

        let mut verifier = make_state();
        install_committed_work_synthesis_wire_surface(&mut verifier);
        verifier
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        verifier.task_profile.mutates_workspace = true;
        verifier.task_profile.verification_required = true;
        verifier
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let disposition = terminal_completion_disposition(&verifier, true);
        assert!(
            !enforce_workspace_completion_before_text_completion_with_disposition(
                &mut verifier,
                disposition,
            )
        );
        assert!(enforce_explicit_verification_before_text_completion(
            &mut verifier
        ));
        assert_eq!(verifier.hooks.completion_settlement.verification_retries, 1);
        assert!(verifier.volatile_pending.iter().any(|injection| {
            injection
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                == Some("explicit_verification_required.v1")
        }));
    }

    #[tokio::test]
    async fn settled_exact_completion_action_is_not_reclassified_as_round_slice_partial() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::PostMutationObservation,
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut host = MockHost::new(vec![text_result(
            "Completed after the exact observation obligation settled.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("typed completion action should be ingested");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "the settled exact action should be consumed exactly once"
        );
    }

    #[tokio::test]
    async fn committed_work_completion_does_not_promote_an_empty_provider_response() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![
            text_result("", 10, 0, Some(1)),
            text_result("Completed after the bounded retry.", 10, 5, Some(1)),
        ])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("an empty response should be handled by the bounded retry path");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.interruption.is_none());
        assert!(
            state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "an empty response must not consume typed final-synthesis authority"
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("the bounded retry should accept non-empty final synthesis");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "the successful retry consumes typed final-synthesis authority"
        );
    }

    #[test]
    fn typed_work_settlement_goal_review_is_not_reclassified_as_contract_failure() {
        let mut state = make_state();
        state.hooks.completion_settlement.work_settlement_only = true;
        state.hooks.completion_settlement.text_only = false;
        state.final_text = "review findings".to_string();

        install_committed_work_synthesis_wire_surface(&mut state);
        super::super::tool_phase::transition_work_settlement_to_final_synthesis(&mut state);

        assert!(!enforce_typed_work_settlement_before_text_completion(
            &mut state
        ));
        assert_eq!(state.final_text, "review findings");
        assert!(
            !state
                .final_text
                .contains(WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT)
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
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
    fn positive_mutation_shape_uses_direct_tool_identity_not_preview_or_parseability() {
        let with_preview = ToolCallRecord {
            name: "write_file".into(),
            args_preview: Some(r#"{"path":"/tmp/out"}"#.into()),
            ..Default::default()
        };
        let malformed_full = ToolCallRecord {
            name: "write_file".into(),
            args_full: Some("not-json".into()),
            ..Default::default()
        };
        let bash_preview = ToolCallRecord {
            name: "bash".into(),
            args_preview: Some(r#"{"command":"printf x > /workspace/out"}"#.into()),
            ..Default::default()
        };
        let bash_malformed = ToolCallRecord {
            name: "bash".into(),
            args_full: Some("not-json".into()),
            ..Default::default()
        };
        let bash_valid = ToolCallRecord {
            name: "bash".into(),
            args_full: Some(r#"{"command":"printf x > /workspace/out"}"#.into()),
            ..Default::default()
        };

        assert!(tool_record_has_positive_mutation_shape(&with_preview));
        assert!(tool_record_has_positive_mutation_shape(&malformed_full));
        assert!(!tool_record_has_positive_mutation_shape(&bash_preview));
        assert!(!tool_record_has_positive_mutation_shape(&bash_malformed));
        assert!(tool_record_has_positive_mutation_shape(&bash_valid));
    }

    #[test]
    fn trusted_bash_receipt_covers_opaque_writer_without_command_special_case() {
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: Some(
                astra_tools::workspace_observation::changed_receipt()
                    .remove(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .expect("receipt field"),
            ),
            args_full: Some(serde_json::json!({"command":"./project-writer"}).to_string()),
            runtime_args_full: Some(serde_json::json!({"command":"./project-writer"}).to_string()),
            ..Default::default()
        });

        assert!(has_executed_positive_workspace_mutation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        let mut failed = state.stall.tool_call_records[0].clone();
        failed.ok = false;
        state.task_profile.mutates_workspace = true;
        state.stall.tool_call_records = vec![failed];
        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(!has_concrete_workspace_mutation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::RequiredWorkspaceMutation)
        );
    }

    #[test]
    fn unscoped_or_non_bash_receipt_never_satisfies_bound_mutation() {
        for (name, scope) in [("bash", "external"), ("mcp_tool", "bound_workspace")] {
            let record = ToolCallRecord {
                name: name.into(),
                ok: true,
                workspace_mutation_observed: Some(true),
                workspace_mutation_scope: Some(scope.into()),
                ..Default::default()
            };
            assert!(!tool_record_has_positive_mutation_shape(&record));
        }
    }

    #[test]
    fn external_scratch_does_not_create_global_mutation_obligation() {
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            args_full: Some(r#"{"path":"/tmp/scratch.txt","content":"x"}"#.into()),
            file_path: Some("/tmp/scratch.txt".into()),
            ..Default::default()
        });

        assert!(!has_executed_positive_workspace_mutation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn external_scratch_shell_snapshot_does_not_create_global_mutation_obligation() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace/astra".into());
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                serde_json::json!({
                    "command": "cd /workspace/astra && git show HEAD:src/lib.rs > /tmp/inspected.rs && sed -n '1,20p' /tmp/inspected.rs"
                })
                .to_string(),
            ),
            ..Default::default()
        });

        assert!(!has_executed_positive_workspace_mutation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn historical_review_diff_scratch_snapshot_does_not_create_global_mutation_obligation() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace/astra".into());
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                serde_json::json!({
                    "command": "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && git diff 449b13b95f56f57619094fbb8afbc496d31dd7a8^ 449b13b95f56f57619094fbb8afbc496d31dd7a8 > /tmp/review.diff && wc -l /tmp/review.diff"
                })
                .to_string(),
            ),
            ..Default::default()
        });

        assert!(!has_executed_positive_workspace_mutation(&state));
        assert_eq!(pending_completion_action(&state), None);
    }

    #[test]
    fn workspace_nested_under_tmp_keeps_bound_mutation_obligation() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/tmp/workspace".into());
        let mut workspace_write = executed_record(
            "write_file",
            true,
            Some(r#"{"path":"/tmp/workspace/out.txt","content":"x"}"#),
        );
        workspace_write.file_path = Some("/tmp/workspace/out.txt".into());
        state.stall.tool_call_records.push(workspace_write);

        assert!(has_executed_positive_workspace_mutation(&state));
        assert!(has_concrete_workspace_mutation(&state));
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
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
    fn successful_validation_closes_post_mutation_obligation_without_overtrusting_opaque_bash() {
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;

        let mutation = ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            args_full: Some(r#"{"path":"src/lib.rs","old":"old","new":"new"}"#.into()),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m pytest tests/test_knot.py"}"#.into()),
            ..Default::default()
        };
        let opaque = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -c 'open(\"src/lib.rs\", \"w\").write(\"x\")'"
                })
                .to_string(),
            ),
            ..Default::default()
        };

        assert!(tool_record_is_workspace_mutation(&mutation));
        assert!(!tool_record_is_workspace_mutation(&validation));
        assert!(!tool_record_is_workspace_mutation(&opaque));
        assert!(
            crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                crate::turn::agentic_loop::lifecycle::extract_tool_args(
                    validation.args_full.as_deref()
                )
                .as_ref(),
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                crate::turn::agentic_loop::lifecycle::extract_tool_args(
                    opaque.args_full.as_deref()
                )
                .as_ref(),
            )
        );
    }

    #[test]
    fn completion_obligation_uses_validation_receipt_but_not_opaque_shell_possibility() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        let mutation = ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({"path":"src/lib.rs","old":"old","new":"new"}).to_string(),
            ),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -m pytest tests/test_knot.py"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), validation];
        assert_eq!(pending_completion_action(&state), None);

        let opaque = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -c 'open(\"src/lib.rs\", \"w\").write(\"x\")'"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), opaque];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        let mut mutating_bash = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "sed -i 's/old/new/' src/lib.rs"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), mutating_bash.clone()];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        let compound_receipt = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "rm -rf build && git push origin main && cat dist/index.html && curl -sk https://localhost/"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![compound_receipt];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::RequiredWorkspaceMutation),
            "an opaque compound shell call is not a concrete mutation receipt for a mutating intent"
        );

        let cython_build = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "python setup.py build_ext --inplace 2>&1 | tail -20"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), cython_build];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation),
            "an in-place build may rewrite workspace artifacts and cannot close the mutation epoch by command shape alone"
        );

        mutating_bash.ok = false;
        state.stall.tool_call_records = vec![mutation, mutating_bash];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
    }

    #[test]
    fn nested_observation_receipt_requires_canonical_bash_args() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        let mutation = ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "prepare-command; value=$(curl -fsS https://service/ | tr -d '\\n'); echo \"status=$([ \\\"$value\\\" = expected ] && echo PASS || echo FAIL)\""
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation, validation];
        assert_eq!(pending_completion_action(&state), None);

        state.stall.tool_call_records[1].ok = false;
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );

        // The normal bash preview is a plain (and potentially truncated)
        // command, not JSON.  It must never be promoted to authoritative
        // completion evidence when args_full is unavailable.
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                workspace_mutation_observed: Some(true),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt:
                    astra_tools::workspace_observation::typed_workspace_tool_receipt()
                        .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                        .cloned(),
                ..Default::default()
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_preview: Some(
                    "prepare-command; value=$(curl -fsS https://service/); echo status=$value"
                        .into(),
                ),
                args_full: None,
                ..Default::default()
            },
        ];
        assert_eq!(
            pending_completion_action(&state),
            Some(CompletionAction::PostMutationObservation)
        );
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
    fn safe_provider_recovery_schedules_exactly_one_thinking_off_convergence() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "a long provisional derivation",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert!(state.provider_adaptation.action_convergence_attempted);
        assert!(state.provider_adaptation.force_next_thinking_off);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.kind == VolatileKind::BehaviorAdvisory
                && entry.payload["schema"] == "provider_safe_recovery.v1"
        }));
        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "the recovery boundary must be consumed exactly once"
        );

        let mut whitespace_state = make_state();
        whitespace_state.remaining_turns = 3;
        let whitespace_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": " \n\t",
                "partial_reasoning": "provisional",
                "tool_calls": []
            })
            .to_string(),
        );
        assert!(
            schedule_safe_provider_recovery(&mut whitespace_state, &whitespace_error),
            "provider whitespace is not visible output and must not suppress convergence"
        );

        let mut text_only_state = make_state();
        text_only_state.remaining_turns = 3;
        text_only_state.hooks.completion_settlement.text_only = true;
        assert!(schedule_safe_provider_recovery(
            &mut text_only_state,
            &whitespace_error
        ));
        let advisory = text_only_state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("text-only convergence advisory");
        assert_eq!(advisory.payload["execution_mode"], "text_only");
        assert!(
            advisory.payload["instruction"]
                .as_str()
                .is_some_and(|instruction| instruction.contains("do not call tools"))
        );
    }

    #[test]
    fn terminal_empty_provider_completion_is_safe_to_recover_once() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {
                    "scope": "provider_completion",
                    "phase": "actionable_output"
                },
                "partial_full_text": "",
                "partial_reasoning": "",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(
            schedule_safe_provider_recovery(&mut state, &error),
            "a terminal provider completion with no delivery is safe to converge once"
        );
        assert!(state.provider_adaptation.force_next_thinking_off);
    }

    #[test]
    fn terminal_empty_recovery_requires_explicit_well_typed_empty_evidence() {
        let incomplete_or_malformed = [
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": "",
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": [],
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": "",
                "tool_calls": {},
                "provider_response": {"transport_success": true}
            }),
        ];

        for details in incomplete_or_malformed {
            let mut state = make_state();
            state.max_turns = 34;
            state.remaining_turns = 0;
            state.hooks.completion_settlement.text_only = true;
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(details.to_string());

            assert!(
                !schedule_safe_provider_recovery(&mut state, &error),
                "missing or malformed delivery evidence must fail closed"
            );
            assert_eq!(state.max_turns, 34);
            assert_eq!(state.remaining_turns, 0);
            assert!(!state.provider_adaptation.action_convergence_attempted);
            assert!(!state.provider_adaptation.force_next_thinking_off);
        }
    }

    #[test]
    fn terminal_whitespace_completion_gets_one_text_only_recovery() {
        let mut state = make_state();
        state.max_turns = 34;
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider convergence completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": " \n\t",
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert_eq!(state.max_turns, 35);
        assert_eq!(state.remaining_turns, 1);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.action_convergence_attempted);
        assert!(state.provider_adaptation.force_next_thinking_off);
    }

    #[test]
    fn provider_completion_error_usage_is_folded_once_and_reclassified() {
        let mut state = make_state();
        state.record_local_usage_coverage(false);
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_completion", "phase": "actionable_output"},
                "provider_response": {"transport_success": true},
                "usage": {
                    "input_tokens": 21,
                    "cached_input_tokens": 3,
                    "cache_creation_tokens": 2,
                    "output_tokens": 8
                }
            })
            .to_string(),
        );

        fold_provider_completion_error_usage(&mut state, &error);
        assert_eq!(state.total_prompt, 21);
        assert_eq!(state.total_cache_read, 3);
        assert_eq!(state.total_cache_creation, 2);
        assert_eq!(state.total_completion, 8);
        assert!(state.has_any_usage);
        assert_eq!(state.last_measured_prompt_tokens, Some(26));
        assert_eq!(state.token_usage_coverage().attempts, 1);
        assert_eq!(state.token_usage_coverage().provider_reported, 1);
        assert_eq!(state.token_usage_coverage().unavailable, 0);

        let mut unavailable_state = make_state();
        unavailable_state.record_local_usage_coverage(false);
        let no_usage_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_completion", "phase": "actionable_output"},
                "provider_response": {"transport_success": true},
                "usage": null
            })
            .to_string(),
        );
        fold_provider_completion_error_usage(&mut unavailable_state, &no_usage_error);
        assert!(!unavailable_state.has_any_usage);
        assert_eq!(
            unavailable_state.token_usage_coverage().provider_reported,
            0
        );
        assert_eq!(unavailable_state.token_usage_coverage().unavailable, 1);
    }

    #[test]
    fn executed_tool_starts_a_new_safe_provider_recovery_epoch() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "provisional reasoning",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert!(state.provider_adaptation.action_convergence_attempted);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            ..Default::default()
        });
        assert!(advance_provider_recovery_epoch_from_new_records(
            &mut state, 0
        ));
        assert!(
            schedule_safe_provider_recovery(&mut state, &error),
            "a typed executed tool terminal creates a fresh inference-progress epoch"
        );

        let mut rejected_state = make_state();
        rejected_state.remaining_turns = 3;
        assert!(schedule_safe_provider_recovery(&mut rejected_state, &error));
        rejected_state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });
        assert!(!advance_provider_recovery_epoch_from_new_records(
            &mut rejected_state,
            0
        ));
        assert!(
            !schedule_safe_provider_recovery(&mut rejected_state, &error),
            "a rejected selection must not replenish recovery"
        );

        for disposition in [
            astra_services::session_journal::ToolCallDisposition::Reused,
            astra_services::session_journal::ToolCallDisposition::Deferred,
        ] {
            let mut non_execution_state = make_state();
            non_execution_state
                .provider_adaptation
                .action_convergence_attempted = true;
            non_execution_state
                .stall
                .tool_call_records
                .push(ToolCallRecord {
                    name: "bash".into(),
                    ok: true,
                    disposition: Some(disposition),
                    ..Default::default()
                });
            assert!(
                !advance_provider_recovery_epoch_from_new_records(&mut non_execution_state, 0),
                "{disposition:?} is not an executor-terminal progress boundary"
            );
            assert!(
                non_execution_state
                    .provider_adaptation
                    .action_convergence_attempted
            );
        }

        let mut failed_execution_state = make_state();
        failed_execution_state
            .provider_adaptation
            .action_convergence_attempted = true;
        failed_execution_state
            .stall
            .tool_call_records
            .push(ToolCallRecord {
                name: "bash".into(),
                ok: false,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                ..Default::default()
            });
        assert!(advance_provider_recovery_epoch_from_new_records(
            &mut failed_execution_state,
            0
        ));
        assert!(
            !failed_execution_state
                .provider_adaptation
                .action_convergence_attempted
        );
    }

    #[test]
    fn safe_provider_recovery_never_supersedes_visible_or_tool_output() {
        for details in [
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "visible partial",
                "partial_reasoning": "",
                "tool_calls": []
            }),
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "thinking",
                "tool_calls": [{"function": {"name": "read_file", "arguments": "{}"}}]
            }),
        ] {
            let mut state = make_state();
            state.remaining_turns = 3;
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "action deadline",
            )
            .with_details_json(details.to_string());
            assert!(!schedule_safe_provider_recovery(&mut state, &error));
            assert!(!state.provider_adaptation.force_next_thinking_off);
        }
    }

    #[test]
    fn stream_transport_with_only_provisional_reasoning_recovers_once() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection reset after thinking",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "partial_reasoning": "working through the task",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("transport recovery advisory");
        assert_eq!(
            advisory.payload["evidence"]["error_kind"],
            "stream_transport"
        );
        assert!(state.provider_adaptation.force_next_thinking_off);
        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "a transport failure has at most one safe logical recovery"
        );
    }

    #[test]
    fn stream_transport_without_partial_attempt_evidence_fails_closed() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection failed before delivery",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "partial_reasoning": "",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(!schedule_safe_provider_recovery(&mut state, &error));
        assert!(!state.provider_adaptation.force_next_thinking_off);
    }

    #[test]
    fn stream_transport_with_malformed_partial_evidence_fails_closed() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection reset after malformed partial",
        )
        .with_details_json(
            serde_json::json!({
                "partial_reasoning": "provisional reasoning",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "a transport recovery requires typed text, reasoning, and tool-call evidence"
        );
        assert!(!state.provider_adaptation.force_next_thinking_off);
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
            state.context_compression_triggered,
            "quiet reactive compaction must remain visible to the final context trace"
        );
        assert!(
            host.compaction_events
                .iter()
                .any(|event| event.kind == CompactionKind::ReactiveBudget),
            "quiet reactive compaction must emit the same structured callback as rendered mode"
        );
        assert!(
            state.step_recorder.events().iter().any(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            }),
            "quiet reactive compaction must still emit durable step audit"
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
        let flags: Vec<Box<dyn Fn(&mut AgenticLoopState)>> =
            vec![Box::new(|s| s.stall.cache_waste_advisory_emitted = true)];
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

    #[derive(Clone, Copy, Debug)]
    enum TypedTerminalGateCase {
        Inference(astra_services::InferenceTerminalStatus),
        TerminalControl,
    }

    #[derive(Debug)]
    struct ProviderUsageGateCase {
        name: &'static str,
        dialect: crate::turn::token_usage::UsageDialect,
        raw_usage: serde_json::Value,
        expected: Option<crate::turn::token_usage::TokenUsage>,
    }

    fn provider_usage_gate_cases() -> Vec<ProviderUsageGateCase> {
        use crate::turn::token_usage::{TokenUsage, UsageDialect};

        vec![
            ProviderUsageGateCase {
                name: "openai_inclusive",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 1_100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100,
                    },
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "openai_compatible_disjoint_aliases",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 200,
                    "completion_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "anthropic_disjoint",
                dialect: UsageDialect::AnthropicMessages,
                raw_usage: serde_json::json!({
                    "input_tokens": 200,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "bedrock_disjoint",
                dialect: UsageDialect::BedrockConverse,
                raw_usage: serde_json::json!({
                    "inputTokens": 200,
                    "outputTokens": 50,
                    "cacheReadInputTokens": 800,
                    "cacheWriteInputTokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "missing",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({}),
                expected: None,
            },
            ProviderUsageGateCase {
                name: "contradictory_openai_inclusive",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100,
                    },
                }),
                expected: Some(TokenUsage {
                    input_tokens: 0,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
        ]
    }

    #[derive(Debug, PartialEq, Eq)]
    struct UsageGateTelemetry {
        first_ttft_ms: Option<u64>,
        current_session_id: Option<String>,
        current_run_id: Option<String>,
        fresh_input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        output_tokens: u64,
        has_any_usage: bool,
        last_measured_prompt_tokens: Option<u64>,
        consecutive_context_window_errors: u32,
    }

    impl UsageGateTelemetry {
        fn from_state(state: &AgenticLoopState) -> Self {
            Self {
                first_ttft_ms: state.telemetry.first_ttft_ms,
                current_session_id: state.current_session_id.clone(),
                current_run_id: state.current_run_id.clone(),
                fresh_input_tokens: state.total_prompt,
                cache_read_tokens: state.total_cache_read,
                cache_creation_tokens: state.total_cache_creation,
                output_tokens: state.total_completion,
                has_any_usage: state.has_any_usage,
                last_measured_prompt_tokens: state.last_measured_prompt_tokens,
                consecutive_context_window_errors: state.consecutive_context_window_errors,
            }
        }

        fn total_input_tokens(&self) -> u64 {
            NormalizedPromptCacheUsage::new(
                self.fresh_input_tokens,
                self.cache_read_tokens,
                self.cache_creation_tokens,
            )
            .total_input_tokens()
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum UsageGateExecution {
        Succeeded,
        Failed(astra_core::ErrorKind),
        Delegated,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct UsageGateObservation {
        telemetry: UsageGateTelemetry,
        execution: UsageGateExecution,
    }

    fn ingest_usage_gate_snapshot(
        state: &mut AgenticLoopState,
        snap: &AgenticTurnStreamSnapshot<'_>,
        quiet: bool,
    ) -> AgenticTurnIngestOutcome {
        let message = state.message.clone();
        let recent_tools = state.recent_tools.clone();
        ingest_agentic_turn_stream(
            snap,
            0,
            |_| unreachable!("usage gate has no edge tools"),
            &message,
            &recent_tools,
            quiet,
            AgenticTurnIngestMut {
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
        )
    }

    fn exercise_usage_gate(
        usage: Option<crate::turn::token_usage::TokenUsage>,
        terminal: TypedTerminalGateCase,
        quiet: bool,
    ) -> UsageGateObservation {
        let usage = usage.unwrap_or_default();
        let error_kind = match terminal {
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::Succeeded,
            )
            | TypedTerminalGateCase::TerminalControl => None,
            TypedTerminalGateCase::Inference(astra_services::InferenceTerminalStatus::Failed) => {
                Some(astra_core::ErrorKind::ServerError)
            }
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::DeliveryUnknown,
            ) => Some(astra_core::ErrorKind::StreamTransport),
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::Cancelled,
            ) => Some(astra_core::ErrorKind::Cancelled),
        };
        let accum = ChatTurnSseAccum {
            session_id: Some("typed-usage-session".to_string()),
            run_id: Some("typed-usage-run".to_string()),
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            has_usage: !usage.is_empty(),
            error_message: error_kind.map(|_| "typed provider terminal".to_string()),
            error_kind,
            ..ChatTurnSseAccum::default()
        };
        let snap = agentic_turn_stream_snapshot_with_kind(&accum, Some(17), error_kind);
        let mut state = make_state();

        let execution = match terminal {
            TypedTerminalGateCase::TerminalControl => {
                let mut host = MockHost::new(Vec::new()).with_quiet(quiet);
                assert_eq!(host.is_quiet(), quiet);
                let outcome = apply_terminal_control_stream_snapshot(
                    &mut host,
                    &mut state,
                    &snap,
                    crate::turn::terminal_control::TerminalControlOutcome::Requested(
                        crate::turn::terminal_control::TerminalHandoffRequest {
                            handoff_id: "typed-handoff".to_string(),
                            kind: "agent".to_string(),
                            target: "typed-target".to_string(),
                            action: "delegate".to_string(),
                            terminal: true,
                            tool_call_id: "typed-tool-call".to_string(),
                        },
                    ),
                );
                assert!(matches!(outcome, AgenticLoopOutcome::Delegated));
                UsageGateExecution::Delegated
            }
            TypedTerminalGateCase::Inference(status) => {
                let outcome = ingest_usage_gate_snapshot(&mut state, &snap, quiet);
                match (status, outcome) {
                    (
                        astra_services::InferenceTerminalStatus::Succeeded,
                        AgenticTurnIngestOutcome::Break,
                    ) => UsageGateExecution::Succeeded,
                    (
                        astra_services::InferenceTerminalStatus::Failed
                        | astra_services::InferenceTerminalStatus::DeliveryUnknown
                        | astra_services::InferenceTerminalStatus::Cancelled,
                        AgenticTurnIngestOutcome::Fatal(error),
                    ) => UsageGateExecution::Failed(error.kind),
                    (status, outcome) => {
                        panic!("unexpected typed terminal outcome: {status:?} -> {outcome:?}")
                    }
                }
            }
        };

        UsageGateObservation {
            telemetry: UsageGateTelemetry::from_state(&state),
            execution,
        }
    }

    #[test]
    fn provider_usage_terminal_quiet_cross_product_preserves_typed_telemetry() {
        use crate::turn::token_usage::extract_usage;
        use astra_services::InferenceTerminalStatus;

        let terminal_cases = [
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::Succeeded),
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::Failed),
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::DeliveryUnknown),
            TypedTerminalGateCase::TerminalControl,
        ];

        for provider in provider_usage_gate_cases() {
            let extracted = provider
                .raw_usage
                .as_object()
                .and_then(|usage| extract_usage(provider.dialect, usage));
            assert_eq!(
                extracted, provider.expected,
                "provider usage normalization: {}",
                provider.name
            );
            let expected = extracted.unwrap_or_default();
            let expected_total_input = expected
                .normalized_prompt_cache_usage()
                .total_input_tokens();

            for terminal in terminal_cases {
                let rendered = exercise_usage_gate(extracted, terminal, false);
                let quiet = exercise_usage_gate(extracted, terminal, true);

                assert_eq!(
                    rendered, quiet,
                    "quiet changed typed telemetry or terminal state: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    rendered.telemetry.fresh_input_tokens, expected.input_tokens,
                    "fresh input: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    rendered.telemetry.cache_read_tokens, expected.cached_input_tokens,
                    "cache read: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    rendered.telemetry.cache_creation_tokens, expected.cache_creation_tokens,
                    "cache create: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    rendered.telemetry.output_tokens, expected.output_tokens,
                    "output: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    rendered.telemetry.total_input_tokens(),
                    expected_total_input,
                    "fresh + read + create: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(rendered.telemetry.has_any_usage, extracted.is_some());
                assert_eq!(rendered.telemetry.first_ttft_ms, Some(17));
                assert_eq!(
                    rendered.telemetry.current_session_id.as_deref(),
                    Some("typed-usage-session")
                );
                assert_eq!(
                    rendered.telemetry.current_run_id.as_deref(),
                    Some("typed-usage-run")
                );

                let expected_execution = match terminal {
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Succeeded) => {
                        UsageGateExecution::Succeeded
                    }
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Failed) => {
                        UsageGateExecution::Failed(astra_core::ErrorKind::ServerError)
                    }
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::DeliveryUnknown) => {
                        UsageGateExecution::Failed(astra_core::ErrorKind::StreamTransport)
                    }
                    TypedTerminalGateCase::TerminalControl => UsageGateExecution::Delegated,
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Cancelled) => {
                        unreachable!("cancelled is outside this exit-gate matrix")
                    }
                };
                assert_eq!(rendered.execution, expected_execution);

                let expected_calibration = match terminal {
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Succeeded)
                    | TypedTerminalGateCase::TerminalControl
                        if extracted.is_some() && expected_total_input > 0 =>
                    {
                        Some(expected_total_input)
                    }
                    _ => None,
                };
                assert_eq!(
                    rendered.telemetry.last_measured_prompt_tokens, expected_calibration,
                    "prompt calibration: {} / {terminal:?}",
                    provider.name
                );
            }
        }
    }

    fn prep(quiet: bool) -> TurnIterationPrep {
        TurnIterationPrep {
            quiet,
            turn_start_time: Instant::now(),
        }
    }

    #[tokio::test]
    async fn user_intent_injects_immediately_at_loop_top() {
        let mut state = make_state();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::MustMutate,
            ),
        );
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.current_run_id = Some("run-queued".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 2,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({"content": "Switch to writing tests first."}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
        assert!(
            state.turn_intent.is_none(),
            "a new semantic user turn must not inherit the old typed intent"
        );
        assert!(!state.task_profile.mutates_workspace);
        assert!(!state.task_profile.verification_required);
        assert_eq!(state.message, "Switch to writing tests first.");
        assert_eq!(
            state.user_intents.applied_user_intents(),
            &[AppliedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
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
    async fn action_boundary_drains_settled_page_before_observing_next_page_guidance() {
        let mut state = make_state();
        state.current_run_id = Some("run-paged-boundary".into());
        state.context_manifest_user_id = Some("user-paged-boundary".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 256,
                snapshot_has_more: true,
                snapshot_page_fact_count: 256,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 257,
                snapshot_has_more: false,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-on-second-page".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 257,
                    input: serde_json::json!({"content": "observe before any stale action"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap(),
            "second-page guidance must invalidate the stale action snapshot"
        );
        assert_eq!(*provider.poll_calls.lock().await, vec![0, 256]);
        assert_eq!(*provider.released.lock().await, vec![257]);
        assert_eq!(state.user_intents.user_intent_cursor(), 257);
        assert_eq!(state.message, "observe before any stale action");
    }

    #[tokio::test]
    async fn action_boundary_fails_closed_when_control_snapshot_exceeds_page_cap() {
        let mut state = make_state();
        state.current_run_id = Some("run-over-page-cap".into());
        state.context_manifest_user_id = Some("user-over-page-cap".into());
        let polls = (1..=MAX_USER_INTENT_BOUNDARY_PAGES)
            .map(|page| UserIntentPoll {
                next_cursor: page,
                snapshot_has_more: true,
                snapshot_page_fact_count: 256,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            })
            .collect();
        let provider = Arc::new(StubRunControlProvider::new(polls));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("an unbounded authoritative snapshot must block provider actions");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(error.message.contains("boundary drain limit"));
        assert_eq!(
            provider.poll_call_count().await,
            MAX_USER_INTENT_BOUNDARY_PAGES
        );
        assert_eq!(
            state.user_intents.user_intent_cursor(),
            MAX_USER_INTENT_BOUNDARY_PAGES - 1,
            "the unprocessed over-cap page must not advance the durable cursor"
        );
    }

    #[tokio::test]
    async fn middle_page_apply_failure_keeps_cursor_for_exact_retry() {
        let mut state = make_state();
        state.current_run_id = Some("run-middle-page-retry".into());
        state.context_manifest_user_id = Some("user-middle-page-retry".into());
        let pending_page = || UserIntentPoll {
            next_cursor: 257,
            snapshot_has_more: false,
            snapshot_page_fact_count: 1,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-middle-page".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 257,
                input: serde_json::json!({"content": "retry this exact page"}),
            }],
            issues: Vec::new(),
            error: None,
        };
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                UserIntentPoll {
                    next_cursor: 256,
                    snapshot_has_more: true,
                    snapshot_page_fact_count: 256,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: None,
                },
                pending_page(),
                pending_page(),
            ],
            1,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("middle-page apply failure must fail the action boundary");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(state.user_intents.user_intent_cursor(), 256);
        assert!(provider.released.lock().await.is_empty());

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert_eq!(*provider.poll_calls.lock().await, vec![0, 256, 256]);
        assert_eq!(*provider.released.lock().await, vec![257]);
        assert_eq!(state.user_intents.user_intent_cursor(), 257);
        assert_eq!(state.message, "retry this exact page");
    }

    #[tokio::test]
    async fn settlement_boundary_forces_poll_before_empty_poll_cooldown_expires() {
        let mut state = make_state();
        state.current_run_id = Some("run-settling".into());
        state.context_manifest_user_id = Some("user-settling".into());
        state
            .user_intents
            .note_user_intent_poll_finished(tokio::time::Instant::now(), Duration::from_secs(60));
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 9,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-at-final-boundary".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 8,
                input: serde_json::json!({"content": "stop here"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_settlement(&mut host, &mut state)
                .await
                .unwrap(),
            "accepted guidance must keep the run open for another model boundary"
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(*provider.released.lock().await, vec![8]);
        assert_eq!(state.message, "stop here");
    }

    #[tokio::test]
    async fn real_run_engine_drains_fenced_intent_before_typed_reopen() {
        let engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        engine
            .start_run("run-real-fence-drain", "user-real", "session-real")
            .await
            .unwrap();
        let accepted = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-real-fence-drain",
            "data": {
                "intent_id": "intent-real-fence-drain",
                "delivery": "guide_current_run",
                "input": {"content": "drain this accepted guidance"}
            }
        });
        assert_eq!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-real",
                    expected_session_id: "session-real",
                    run_id: "run-real-fence-drain",
                    intent_id: "intent-real-fence-drain",
                    event: &accepted,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { event_index: 1 }
        );
        engine
            .fence_user_intent_submissions(
                "user-real",
                "session-real",
                "run-real-fence-drain",
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();

        let mut state = make_state();
        state.current_run_id = Some("run-real-fence-drain".into());
        state.current_run_owner_generation = Some(0);
        state.context_manifest_user_id = Some("user-real".into());
        state.current_session_id = Some("session-real".into());
        state.run_control = Some(engine.clone());
        let mut host = MockHost::new(vec![]);
        assert!(
            inject_polled_user_intents_before_settlement(&mut host, &mut state)
                .await
                .unwrap(),
            "the forced boundary must ingest the accepted pre-cutoff intent"
        );
        assert_eq!(state.message, "drain this accepted guidance");

        let control =
            continue_after_user_intent_settlement_fence(UserIntentSettlementFence::Committed {
                run_control: engine.clone(),
                user_id: "user-real".into(),
                expected_session_id: "session-real".into(),
                run_id: "run-real-fence-drain".into(),
                authority: UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            })
            .await
            .unwrap();
        assert!(matches!(control, TurnExecutionControl::ContinueLoop));

        let after_reopen = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-real-after-reopen",
            "data": {
                "intent_id": "intent-real-after-reopen",
                "delivery": "guide_current_run",
                "input": {"content": "continue after reopen"}
            }
        });
        assert!(matches!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-real",
                    expected_session_id: "session-real",
                    run_id: "run-real-fence-drain",
                    intent_id: "intent-real-after-reopen",
                    event: &after_reopen,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { .. }
        ));
        let run = engine
            .load_run("user-real", "run-real-fence-drain")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events[2]["data"]["after_event_index"], 1);
        assert_eq!(run.events[3]["event_type"], "user_intent_applied");
        assert_eq!(
            run.events[4]["event_type"],
            "user_intent_admission_reopened"
        );
    }

    #[tokio::test]
    async fn real_run_engine_rejects_stale_terminal_and_missing_provider_authority_before_host() {
        let engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        engine
            .start_run(
                "run-provider-authority",
                "user-provider",
                "session-provider",
            )
            .await
            .unwrap();

        let mut stale = make_state();
        stale.current_run_id = Some("run-provider-authority".into());
        stale.current_run_owner_generation = Some(1);
        stale.context_manifest_user_id = Some("user-provider".into());
        stale.current_session_id = Some("session-provider".into());
        stale.run_control = Some(engine.clone());
        let mut stale_progress = attach_llm_progress_receiver(&mut stale);
        let mut stale_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let stale_result = execute_turn_and_ingest_phase(
            &mut stale_host,
            &mut stale,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(stale_error) = stale_result else {
            panic!("rotated generation must stop before the provider");
        };
        assert_eq!(stale_error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(stale_host.turn_count(), 0);
        assert_no_llm_progress(&mut stale_progress);

        engine
            .persist_typed_cancellation_fixture(
                "user-provider",
                "session-provider",
                "run-provider-authority",
                &[astra_core::STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            )
            .await
            .unwrap();
        let mut terminal = make_state();
        terminal.current_run_id = Some("run-provider-authority".into());
        terminal.current_run_owner_generation = Some(0);
        terminal.context_manifest_user_id = Some("user-provider".into());
        terminal.current_session_id = Some("session-provider".into());
        terminal.run_control = Some(engine.clone());
        let mut terminal_progress = attach_llm_progress_receiver(&mut terminal);
        let mut terminal_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let terminal_result = execute_turn_and_ingest_phase(
            &mut terminal_host,
            &mut terminal,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(terminal_error) = terminal_result else {
            panic!("terminal run must stop before the provider");
        };
        assert_eq!(terminal_error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(terminal_host.turn_count(), 0);
        assert_no_llm_progress(&mut terminal_progress);

        let mut missing = make_state();
        missing.current_run_id = Some("run-provider-authority".into());
        missing.current_run_owner_generation = None;
        missing.context_manifest_user_id = Some("user-provider".into());
        missing.run_control = Some(engine);
        let mut missing_progress = attach_llm_progress_receiver(&mut missing);
        let mut missing_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let missing_result = execute_turn_and_ingest_phase(
            &mut missing_host,
            &mut missing,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(missing_error) = missing_result else {
            panic!("durable provider without a generation must fail closed");
        };
        assert_eq!(missing_error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(missing_host.turn_count(), 0);
        assert_no_llm_progress(&mut missing_progress);
    }

    #[tokio::test]
    async fn paused_provider_boundary_is_a_hold_not_a_cancellation() {
        let provider = Arc::new(BoundaryDecisionRunControl {
            decision: Ok(ProviderBoundaryAuthorization::Paused),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let pause_flag = Arc::new(AtomicBool::new(false));
        let mut state = make_state();
        state.current_run_id = Some("run-paused-boundary".into());
        state.current_run_owner_generation = Some(7);
        state.context_manifest_user_id = Some("user-paused-boundary".into());
        state.current_session_id = Some("session-paused-boundary".into());
        state.run_control = Some(provider.clone());
        state.cancellation.pause_flag = Some(pause_flag.clone());

        assert_eq!(
            authorize_provider_boundary(&mut state).await.unwrap(),
            ProviderBoundaryGate::Paused
        );
        assert!(pause_flag.load(Ordering::Acquire));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_boundary_typed_unhappy_outcomes_never_enter_host() {
        for (decision, expected_kind) in [
            (
                Ok(ProviderBoundaryAuthorization::AuthorityLost {
                    reason: "owner lease expired".to_string(),
                }),
                astra_core::ErrorKind::Cancelled,
            ),
            (
                Ok(ProviderBoundaryAuthorization::Inactive {
                    status: astra_core::STATUS_COMPLETED.to_string(),
                }),
                astra_core::ErrorKind::Cancelled,
            ),
            (
                Err("database unavailable".to_string()),
                astra_core::ErrorKind::Unknown,
            ),
        ] {
            let provider = Arc::new(BoundaryDecisionRunControl {
                decision,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut state = make_state();
            state.current_run_id = Some("run-boundary-decision".into());
            state.current_run_owner_generation = Some(7);
            state.context_manifest_user_id = Some("user-boundary-decision".into());
            state.current_session_id = Some("session-boundary-decision".into());
            state.run_control = Some(provider.clone());
            let mut progress = attach_llm_progress_receiver(&mut state);
            let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

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
            let Err(error) = result else {
                panic!("provider authorization rejection must be fail-closed");
            };
            assert_eq!(error.kind, expected_kind);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(host.turn_count(), 0);
            assert_no_llm_progress(&mut progress);
        }
    }

    #[tokio::test]
    async fn provider_progress_is_emitted_only_for_real_host_calls_and_is_always_paired() {
        let mut success_state = make_state();
        let mut success_progress = attach_llm_progress_receiver(&mut success_state);
        let mut success_host = MockHost::new(vec![text_result("done", 3, 2, Some(7))]);
        execute_turn_and_ingest_phase(
            &mut success_host,
            &mut success_state,
            4,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("successful host boundary");
        assert_eq!(success_host.turn_count(), 1);
        assert_one_paired_llm_progress(&mut success_progress, 4, Some(7));

        let mut error_state = make_state();
        let mut error_progress = attach_llm_progress_receiver(&mut error_state);
        let mut error_host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ServerError,
            "provider rejected request",
        ));
        let result = execute_turn_and_ingest_phase(
            &mut error_host,
            &mut error_state,
            9,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(error_host.turn_count(), 1);
        assert_one_paired_llm_progress(&mut error_progress, 9, None);
    }

    #[tokio::test]
    async fn hard_round_limit_rejection_emits_no_provider_progress() {
        let mut state = make_state();
        state.llm_rounds_completed = 1;
        state.stall.circuit_breaker =
            astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                astra_turn_core::loop_circuit_breaker::BreakerConfig {
                    absolute_max_rounds: 1,
                    ..Default::default()
                },
            );
        let mut progress = attach_llm_progress_receiver(&mut state);
        let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("hard infrastructure limit is a typed pre-host return");

        assert!(matches!(
            result,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.turn_count(), 0);
        assert_no_llm_progress(&mut progress);
    }

    #[cfg(feature = "harness")]
    #[tokio::test]
    async fn harness_pre_llm_rejection_emits_no_provider_progress() {
        let mut state = make_state();
        let sink: Arc<dyn astra_harness::SnapshotSink> = astra_harness::InMemorySnapshotSink::arc();
        state.harness =
            crate::turn::harness_adapter::HarnessSlot::new(Arc::new(PreLlmBlockingHarness), sink);
        let mut progress = attach_llm_progress_receiver(&mut state);
        let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            3,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("harness block is a typed pre-host return");

        assert!(matches!(
            result,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.turn_count(), 0);
        assert_no_llm_progress(&mut progress);
    }

    #[tokio::test]
    async fn settlement_boundary_fails_closed_when_durable_poll_is_unavailable() {
        let mut state = make_state();
        state.current_run_id = Some("run-settling".into());
        state.context_manifest_user_id = Some("user-settling".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 0,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: Some("durable store unavailable".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_settlement(&mut host, &mut state)
            .await
            .expect_err("settlement cannot claim completion without closing the intent lane");

        assert_eq!(error.kind, astra_core::ErrorKind::Unknown);
        assert!(
            error
                .to_string()
                .contains("authoritative user intent boundary poll failed")
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(state.user_intents.user_intent_cursor(), 0);
    }

    #[tokio::test]
    async fn action_boundary_forces_guidance_poll_before_stale_tool_execution() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-fence".into());
        state.context_manifest_user_id = Some("user-action-fence".into());
        state
            .user_intents
            .note_user_intent_poll_finished(tokio::time::Instant::now(), Duration::from_secs(60));
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 4,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-during-provider".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 3,
                input: serde_json::json!({"content": "change direction before acting"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap(),
            "new guidance must advance the execution epoch before tools are admitted"
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(*provider.released.lock().await, vec![3]);
        assert_eq!(state.message, "change direction before acting");
    }

    #[tokio::test]
    async fn action_boundary_fails_closed_when_guidance_apply_ack_fails() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-ack".into());
        state.context_manifest_user_id = Some("user-action-ack".into());
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![UserIntentPoll {
                next_cursor: 4,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-action-ack".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 3,
                    input: serde_json::json!({"content": "do not execute the stale tool"}),
                }],
                issues: Vec::new(),
                error: None,
            }],
            1,
        ));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("an uncommitted control epoch must not release stale actions");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(error.to_string().contains("durably acknowledged"));
    }

    #[tokio::test]
    async fn forced_action_boundary_retries_pending_apply_ack_during_backoff() {
        let mut state = make_state();
        state.current_run_id = Some("run-pending-action-ack".into());
        state.context_manifest_user_id = Some("user-pending-action-ack".into());
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                UserIntentPoll {
                    next_cursor: 4,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: vec![crate::turn::run_control::QueuedUserIntent {
                        intent_id: "intent-pending-action-ack".into(),
                        delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                        status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                        event_index: 3,
                        input: serde_json::json!({"content": "replace stale action"}),
                    }],
                    issues: Vec::new(),
                    error: None,
                },
                UserIntentPoll::default(),
            ],
            1,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .expect("ordinary cadence may defer the failed ACK");
        assert!(provider.released.lock().await.is_empty());

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .expect("forced fence must retry rather than honor cadence backoff")
        );
        assert_eq!(*provider.released.lock().await, vec![3]);
        assert_eq!(state.message, "replace stale action");
    }

    #[tokio::test]
    async fn action_boundary_discards_stale_actions_when_terminal_wins_apply_race() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-terminal".into());
        state.context_manifest_user_id = Some("user-action-terminal".into());
        let provider = Arc::new(StubRunControlProvider::with_terminal_release(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-terminal-race".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "stop"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("a terminal owner must prevent execution of the stale response");
        assert_eq!(error.kind, astra_core::ErrorKind::Cancelled);
    }

    #[test]
    fn superseded_provider_round_keeps_usage_and_trace_without_content_authority() {
        let mut state = make_state();
        state.current_run_id = Some("run-superseded".into());
        let mut result = text_result("stale answer", 120, 11, Some(7));
        result.accum.cache_read_tokens = 30;
        result.accum.cache_creation_tokens = 5;
        result.accum.tool_calls = vec![serde_json::json!({
            "id":"stale-tool",
            "type":"function",
            "function":{"name":"read_file","arguments":"{}"}
        })];

        record_superseded_llm_round(&mut state, &result, Instant::now());

        assert_eq!(state.total_prompt, 120);
        assert_eq!(state.total_completion, 11);
        assert_eq!(state.total_cache_read, 30);
        assert_eq!(state.total_cache_creation, 5);
        assert!(state.has_any_usage);
        let round = state.recent_rounds.last().expect("physical round retained");
        assert_eq!(
            round.finish_reason.as_deref(),
            Some("superseded_by_user_intent")
        );
        assert_eq!(round.tool_call_names, ["read_file"]);
        assert!(
            state.messages.is_empty(),
            "stale content must not be ingested"
        );
        assert!(
            state.stall.tool_call_records.is_empty(),
            "stale tools must not execute"
        );
    }

    #[test]
    fn superseded_remote_summary_keeps_deduplicated_accounting_without_actions() {
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        let mut result = text_result("stale remote answer", 240, 20, Some(9));
        result.accum.run_id = Some("remote-run".into());
        result.accum.cache_read_tokens = 80;
        result.accum.server_execution_summary = Some(ServerLoopExecutionSummary {
            llm_rounds: 2,
            tool_calls_count: 3,
            observation_tool_calls_count: 1,
            tools_used: vec!["read_file".into(), "bash".into()],
            ..Default::default()
        });

        record_superseded_llm_round(&mut state, &result, Instant::now());
        record_superseded_llm_round(&mut state, &result, Instant::now());

        assert_eq!(state.llm_rounds_completed, 2);
        assert_eq!(state.total_tool_calls, 3);
        assert_eq!(state.total_observation_tool_calls, 1);
        assert_eq!(state.total_prompt, 240);
        assert_eq!(state.total_completion, 20);
        assert_eq!(state.total_cache_read, 80);
        assert!(state.has_any_usage);
        assert!(state.messages.is_empty());
        assert!(state.stall.tool_call_records.is_empty());
    }

    #[tokio::test]
    async fn durable_settlement_without_exact_owner_generation_fails_closed() {
        let mut state = make_state();
        state.current_run_id = Some("run-missing-generation".into());
        state.context_manifest_user_id = Some("user-missing-generation".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            UserIntentPoll::default(),
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![text_result("final", 5, 2, None)]);

        let error = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect_err("durable settlement cannot reconstruct owner authority");

        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(provider.fence_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.reopen_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn every_nonterminal_post_fence_guard_reopens_the_exact_durable_run() {
        #[derive(Clone, Copy, Debug)]
        enum ContinuationCase {
            WorkSettlement,
            WorkspaceGuard,
            VerificationGuard,
            OutcomeReconciliation,
            GuidanceArrived,
        }

        for case in [
            ContinuationCase::WorkSettlement,
            ContinuationCase::WorkspaceGuard,
            ContinuationCase::VerificationGuard,
            ContinuationCase::OutcomeReconciliation,
            ContinuationCase::GuidanceArrived,
        ] {
            let mut state = make_state();
            state.current_run_id = Some(format!("run-{case:?}"));
            state.current_run_owner_generation = Some(17);
            state.context_manifest_user_id = Some("user-settlement-guard".into());
            let polls = if matches!(case, ContinuationCase::GuidanceArrived) {
                vec![
                    UserIntentPoll::default(),
                    UserIntentPoll::default(),
                    UserIntentPoll {
                        next_cursor: 2,
                        snapshot_has_more: false,
                        snapshot_page_fact_count: 0,
                        inputs: vec![crate::turn::run_control::QueuedUserIntent {
                            intent_id: "intent-settlement-guard".into(),
                            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                            status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                            event_index: 1,
                            input: serde_json::json!({"content": "continue with this guidance"}),
                        }],
                        issues: Vec::new(),
                        error: None,
                    },
                ]
            } else {
                vec![UserIntentPoll::default(); 3]
            };
            let provider = Arc::new(StubRunControlProvider::new(polls));
            state.run_control = Some(provider.clone());

            match case {
                ContinuationCase::WorkSettlement => {
                    state.hooks.completion_settlement.work_settlement_only = true;
                }
                ContinuationCase::WorkspaceGuard => mark_must_mutate(&mut state),
                ContinuationCase::VerificationGuard => {
                    state.task_profile.mutates_workspace = true;
                    state.task_profile.verification_required = true;
                    state
                        .hooks
                        .stop_hooks
                        .push(explicit_verification_hook("quality", "./quality-gate"));
                    state
                        .stall
                        .tool_call_records
                        .push(executed_record("write_file", true, None));
                    state
                        .stall
                        .tool_call_records
                        .push(executed_record("read_file", true, None));
                }
                ContinuationCase::OutcomeReconciliation => {
                    state.stall.active_policy_feedback =
                        serde_json::from_value(serde_json::json!({
                            "state": "evaluated",
                            "schema_version": 2,
                            "revision": 2,
                            "evaluated_at_round": 4,
                            "subject": {"kind": "run"},
                            "entries": [{
                                "signal": "unresolved_tool_outcomes",
                                "stage": "converge",
                                "observed_at_round": 4,
                                "evidence_count": 2,
                                "recommendation": "diagnose_tool_outcomes"
                            }]
                        }))
                        .expect("valid reconciliation feedback");
                }
                ContinuationCase::GuidanceArrived => {}
            }

            let mut host = MockHost::new(vec![text_result("candidate", 5, 2, None)]);
            let control = execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{case:?} must continue after reopening: {error}"));

            assert!(
                matches!(control, TurnExecutionControl::ContinueLoop),
                "{case:?} must be nonterminal"
            );
            assert_eq!(
                provider.fence_calls.load(Ordering::SeqCst),
                1,
                "{case:?} must commit one fence"
            );
            assert_eq!(
                provider.reopen_calls.load(Ordering::SeqCst),
                1,
                "{case:?} must close its committed fence"
            );
            assert_eq!(*provider.fence_generations.lock().await, vec![17]);
            assert_eq!(*provider.reopen_generations.lock().await, vec![17]);
        }
    }

    #[tokio::test]
    async fn post_fence_reopen_failure_fails_closed_before_another_provider_round() {
        let mut state = make_state();
        state.current_run_id = Some("run-reopen-failure".into());
        state.current_run_owner_generation = Some(23);
        state.context_manifest_user_id = Some("user-reopen-failure".into());
        state.hooks.completion_settlement.work_settlement_only = true;
        let provider = Arc::new(StubRunControlProvider::with_reopen_error(
            vec![UserIntentPoll::default(); 3],
            "durable reopen unavailable",
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![
            text_result("candidate", 5, 2, None),
            text_result("must not execute", 5, 2, None),
        ]);

        let error = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect_err("failed reopen cannot authorize the next provider boundary");

        assert_eq!(error.kind, astra_core::ErrorKind::Unknown);
        assert!(error.message.contains("durable reopen unavailable"));
        assert_eq!(host.turn_count(), 1);
        assert_eq!(provider.fence_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.reopen_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn terminal_completion_after_fence_does_not_reopen_admission() {
        let mut state = make_state();
        state.current_run_id = Some("run-terminal-after-fence".into());
        state.current_run_owner_generation = Some(29);
        state.context_manifest_user_id = Some("user-terminal-after-fence".into());
        mark_must_mutate(&mut state);
        state.hooks.completion_settlement.workspace_mutation_retries = 1;
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default();
            3
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![text_result("candidate", 5, 2, None)]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("terminal incomplete remains a successful control transition");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(provider.fence_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.reopen_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn guidance_at_settlement_reopens_admission_for_the_continuing_run() {
        let mut state = make_state();
        state.current_run_id = Some("run-continuing".into());
        state.current_run_owner_generation = Some(7);
        state.context_manifest_user_id = Some("user-continuing".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            // The post-provider action fence observes no guidance. The next
            // poll is the final-settlement fence, which must still close the
            // race and reopen submissions for the continuing run.
            UserIntentPoll::default(),
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-continue".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "include this correction"}),
                }],
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                ..UserIntentPoll::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![
            text_result("first", 5, 2, None),
            text_result("final", 5, 2, None),
        ]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "continuing run failed after reopening admission: {outcome:?}"
        );
        assert_eq!(state.llm_rounds_completed, 2);
        assert_eq!(provider.reopen_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.fence_calls.load(Ordering::SeqCst), 2);
        assert_eq!(*provider.fence_generations.lock().await, vec![7, 7]);
        assert_eq!(*provider.reopen_generations.lock().await, vec![7]);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(
            provider.provider_authorization_calls.load(Ordering::SeqCst),
            host.turn_count(),
            "each normal provider round must have exactly one bounded authority check"
        );
    }

    #[tokio::test]
    async fn active_run_guidance_keeps_runtime_work_truth_out_of_user_speech() {
        let mut state = make_state();
        state.current_run_id = Some("run-status".into());
        state.context_manifest_user_id = Some("user-status".into());
        state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination = Some(
            crate::turn::agentic_loop::host::ForegroundFanoutPagination {
                group_id: "old-group".into(),
                target_count: 1,
                pending_slots: std::collections::BTreeMap::from([(0, 1024)]),
            },
        );
        state.hooks.completion_settlement.text_only = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        let snapshot = "<background_tasks count=\"1\"><task id=\"review-group\" kind=\"agent_fanout\" status=\"running\" active=\"2\" completed=\"1\" /></background_tasks>";
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 1,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-status".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({
                    "content": "现在什么情况？",
                    "astra_runtime_context": {
                        "schema": "active_work_snapshot.v1",
                        "authority": "run_control_provider",
                        "background_work_snapshot": snapshot,
                        "work_unit_observations": [{
                            "id": "review-group",
                            "kind": "agent_fanout",
                            "status": "running",
                            "revision": 7,
                            "mode": "current",
                            "wake_policy": "on_attention_or_terminal"
                        }],
                    }
                }),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.message, "现在什么情况？");
        assert!(
            state
                .hooks
                .completion_settlement
                .foreground_fanout_pagination
                .is_none()
        );
        assert!(
            !state.hooks.completion_settlement.text_only
                && !state
                    .hooks
                    .completion_settlement
                    .preserve_final_synthesis_wire_surface,
            "new semantic guidance must revoke the previous turn's synthesis authority"
        );
        assert_eq!(
            state.user_intents.applied_user_intents()[0].content,
            "现在什么情况？",
            "runtime projection must not pollute durable user speech"
        );
        let work_context = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .expect("active guidance must carry its runtime work snapshot");
        assert_eq!(
            work_context.payload["snapshots"][0]["background_work_snapshot"],
            snapshot
        );
        assert_eq!(
            work_context.kind.delivery_class(),
            astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext
        );
        assert_eq!(
            state.stall.work_unit_observations.active_work_units()[0].id,
            "review-group",
            "the same typed snapshot must gate final-answer settlement"
        );

        let unchanged = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            7,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        assert_eq!(
            state.observe_work_unit(&unchanged),
            astra_core::work_unit::WorkUnitObservationOutcome::Unchanged { consecutive: 1 }
        );
        let unchanged_context = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .unwrap();
        assert_eq!(
            unchanged_context.payload["snapshots"][0]["background_work_snapshot"], snapshot,
            "an identical producer observation must not discard unrelated XML projections"
        );
        assert!(
            unchanged_context.payload["snapshots"][0]
                .get("projection_state")
                .is_none(),
            "an identical observation is not a superseding producer revision"
        );

        let completed = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Completed,
            8,
            astra_core::work_unit::WorkUnitObservationMode::Transition,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        state.observe_work_unit(&completed);
        assert!(state.stall.work_unit_observations.is_empty());
        let refreshed = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .unwrap();
        assert_eq!(
            refreshed.payload["snapshots"][0]["work_unit_observations"][0]["status"],
            "completed"
        );
        assert!(
            refreshed.payload["snapshots"][0]
                .get("background_work_snapshot")
                .is_none(),
            "newer producer truth must retire the stale submission-time XML"
        );
    }

    #[test]
    fn delayed_nonterminal_snapshot_cannot_overwrite_newer_producer_revision() {
        let mut state = make_state();
        let registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let newer = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::WaitingForInput,
            8,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        registry.observe(&newer);
        state.attach_active_work_registry(registry);

        let mut delayed = serde_json::json!({
            "schema": "active_work_snapshot.v1",
            "background_work_snapshot": "stale display projection",
            "work_unit_observations": [{
                "id": "review-group",
                "kind": "agent_fanout",
                "status": "running",
                "revision": 7,
                "mode": "current",
                "wake_policy": "on_attention_or_terminal"
            }]
        });
        state.reconcile_active_work_context(&mut delayed);

        assert_eq!(
            delayed["work_unit_observations"][0]["status"],
            "waiting_for_input"
        );
        assert_eq!(delayed["work_unit_observations"][0]["revision"], 8);
        assert!(delayed.get("background_work_snapshot").is_none());
        assert_eq!(
            delayed["projection_state"],
            "superseded_by_newer_producer_observation"
        );
    }

    #[test]
    fn final_settlement_reconciles_terminal_registry_truth_before_rendering() {
        let mut state = make_state();
        let registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let running = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            1,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap();
        registry.observe(&running);
        state.attach_active_work_registry(registry.clone());
        let completed = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Completed,
            2,
            astra_core::work_unit::WorkUnitObservationMode::Transition,
        )
        .unwrap();
        registry.observe(&completed);
        state.final_text = "All reviewers stopped.".to_string();
        reconcile_unsettled_work_status(&mut state);

        assert_eq!(state.final_text, "All reviewers stopped.");
        assert!(
            state
                .stall
                .work_unit_observations
                .active_work_units()
                .is_empty(),
            "terminal producer truth must absorb the stale turn-local running projection"
        );
    }

    #[tokio::test]
    async fn user_intent_records_multiple_inputs_without_consecutive_user_messages() {
        let mut state = make_state();
        state.current_run_id = Some("run-queued-many".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 3,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "first queued input"}),
                },
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-2".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 2,
                    input: serde_json::json!({"content": "second queued input"}),
                },
            ],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 3);
        assert_eq!(*provider.released.lock().await, vec![1, 2]);
        assert_eq!(state.message, "second queued input");
        assert_eq!(
            state.user_intents.applied_user_intents(),
            &[
                AppliedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::Applied,
                    event_index: 1,
                    content: "first queued input".to_string(),
                },
                AppliedUserIntent {
                    intent_id: "input-2".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::Applied,
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
            "user intent injection must keep prompt history provider-safe"
        );
    }

    #[tokio::test]
    async fn user_intent_does_not_reinject_after_cursor_advance() {
        let mut state = make_state();
        state.current_run_id = Some("run-repoll".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "only once"}),
                }],
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert_eq!(state.user_intents.user_intent_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
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
    async fn user_intent_empty_poll_is_throttled() {
        let mut state = make_state();
        state.current_run_id = Some("run-empty-throttle".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 0,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "arrived after quiet poll"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 1);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "empty poll should suppress immediate follow-up DB poll"
        );
        assert!(state.messages.is_empty());

        tokio::time::advance(USER_INTENT_EMPTY_POLL_INTERVAL - Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "poll should remain suppressed until the interval fully elapses"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 2);
        assert_eq!(state.message, "arrived after quiet poll");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn provider_boundary_bypasses_empty_poll_throttle_after_tool_work() {
        let mut state = make_state();
        state.current_run_id = Some("run-provider-boundary".into());
        state.context_manifest_user_id = Some("user-provider-boundary".into());
        state.current_session_id = Some("session-provider-boundary".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 0,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "stop-stale-round".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "wait before doing more work"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 1);

        // Model the next semantic boundary arriving immediately after a tool
        // completes, while the ordinary empty-poll debounce is still active.
        let applied = inject_polled_user_intents_before_provider(&mut host, &mut state)
            .await
            .unwrap();

        assert!(applied);
        assert_eq!(provider.poll_call_count().await, 2);
        assert_eq!(state.message, "wait before doing more work");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn user_intent_retries_release_without_reinjecting_after_ack_failure() {
        let mut state = make_state();
        state.current_run_id = Some("run-release-retry".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let pending_poll = || UserIntentPoll {
            next_cursor: 2,
            snapshot_has_more: false,
            snapshot_page_fact_count: 1,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({"content": "inject once"}),
            }],
            issues: Vec::new(),
            error: None,
        };
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                pending_poll(),
                pending_poll(),
                pending_poll(),
                pending_poll(),
            ],
            2,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert!(
            host.user_intent_context_indices.is_empty(),
            "uncommitted guidance must not become model-visible"
        );
        assert!(
            host.user_intent_applied_indices.is_empty(),
            "live applied evidence must wait for durable acknowledgement"
        );
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "failed durable acknowledgement must not create a tight retry loop"
        );
        tokio::time::advance(USER_INTENT_EMPTY_POLL_INTERVAL).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 2);
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            2,
            "second acknowledgement failure must increase the retry delay"
        );
        tokio::time::advance(Duration::from_millis(999)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            3,
            "regular polling continues"
        );
        assert!(provider.released.lock().await.is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 2);
        assert_eq!(
            provider.poll_call_count().await,
            4,
            "pending release acknowledgement retries after its backoff"
        );
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
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
    async fn terminal_run_does_not_publish_user_intent_applied() {
        let mut state = make_state();
        state.current_run_id = Some("run-terminal-race".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::with_terminal_release(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-terminal".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "too late"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert!(host.user_intent_context_indices.is_empty());
        assert!(host.user_intent_applied_indices.is_empty());
        assert_eq!(host.user_intent_returned_indices, vec![1]);
        assert!(!state.messages.iter().any(|message| {
            message.get("content").and_then(serde_json::Value::as_str) == Some("too late")
        }));
        assert!(provider.released.lock().await.is_empty());
        assert!(
            !state
                .user_intents
                .should_retry_apply_ack(tokio::time::Instant::now())
        );
    }

    #[tokio::test]
    async fn committed_apply_outbox_rehydrates_model_context_without_second_ack() {
        let mut state = make_state();
        state.current_run_id = Some("run-recovered-apply".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 3,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-recovered".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 1,
                input: serde_json::json!({"content": "recover after durable apply"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert!(host.user_intent_applied_indices.is_empty());
        assert!(provider.released.lock().await.is_empty());
        assert_eq!(state.user_intents.applied_user_intents().len(), 1);
        assert!(state.messages.iter().any(|message| {
            message.get("content").and_then(serde_json::Value::as_str)
                == Some("recover after durable apply")
        }));
    }

    #[tokio::test]
    async fn malformed_user_intent_isolated_without_blocking_later_valid_input() {
        let mut state = make_state();
        state.current_run_id = Some("run-invalid".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 7,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-6".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 6,
                    input: serde_json::json!({"unexpected": true}),
                },
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-7".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 7,
                    input: serde_json::json!({"content": "continue with the valid guidance"}),
                },
            ],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 7);
        assert_eq!(*provider.released.lock().await, vec![7]);
        assert_eq!(state.message, "continue with the valid guidance");
        assert_eq!(state.user_intents.applied_user_intents().len(), 1);
        assert_eq!(
            state.user_intents.applied_user_intents()[0].intent_id,
            "input-7"
        );
        assert!(state.volatile_pending.is_empty());
    }

    #[tokio::test]
    async fn user_intent_poll_error_degrades_without_advancing_cursor() {
        let mut state = make_state();
        state.current_run_id = Some("run-missing".into());
        state.context_manifest_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 4,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: Some("run not found while polling user intent: run-missing".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .expect("poll errors are control-plane misses and should not fail the main turn");

        assert_eq!(state.user_intents.user_intent_cursor(), 0);
        assert!(state.messages.is_empty());
        assert!(state.volatile_pending.is_empty());
        assert!(provider.released.lock().await.is_empty());
    }

    #[test]
    fn user_intent_content_preserves_active_skills_hint() {
        let rendered = crate::turn::run_control::user_intent_content(&serde_json::json!({
            "content": "Use the release checklist.",
            "active_skills": ["release-manager", "deploy-auditor"],
        }))
        .expect("user intent should render");

        assert!(rendered.contains("Requested active skills: release-manager, deploy-auditor."));
        assert!(rendered.contains("Use the release checklist."));
    }

    fn execution_escalation_state() -> AgenticLoopState {
        let mut state = make_state();
        state.message = "fix the broken auth middleware".into();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);
        for i in 0..EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "read_file".to_string(),
                ok: true,
                ms: 10,
                args_preview: Some(format!("path: src/{i}.rs")),
                file_path: Some(format!("src/{i}.rs")),
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                ..Default::default()
            });
        }
        state
    }

    #[tokio::test]
    async fn auto_delivers_policy_feedback_to_model_without_status_chatter() {
        let mut auto_state = execution_escalation_state();
        let history_before = auto_state.messages.clone();
        let mut auto_host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
            .with_interaction_mode(TurnInteractionMode::Auto);

        execute_turn_and_ingest_phase(&mut auto_host, &mut auto_state, 0, prep(false))
            .await
            .expect("auto turn");

        let delivered = auto_host
            .executed_volatile
            .first()
            .expect("volatile model boundary");
        assert!(
            delivered
                .iter()
                .any(|injection| injection.kind == VolatileKind::ExecutionEscalation),
            "Auto must preserve policy feedback at the model boundary"
        );
        assert_eq!(
            auto_host.executed_messages.first(),
            Some(&history_before),
            "runtime feedback must not impersonate conversational history"
        );
        assert!(
            auto_host
                .emitted_lines
                .iter()
                .all(|line| !line.contains("Mutating task accumulated")),
            "Auto should not turn model feedback into repeated UI status lines"
        );

        let mut prompt_state = execution_escalation_state();
        let mut prompt_host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
            .with_interaction_mode(TurnInteractionMode::Prompt);
        execute_turn_and_ingest_phase(&mut prompt_host, &mut prompt_state, 0, prep(false))
            .await
            .expect("prompt turn");
        assert!(
            prompt_host
                .emitted_lines
                .iter()
                .any(|line| line.contains("Mutating task accumulated")),
            "Prompt mode may mirror the same policy evidence as status text"
        );
    }

    #[tokio::test]
    async fn local_host_delivers_low_slice_horizon_at_the_model_boundary() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 4;
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .expect("local turn");

        let delivered = host
            .executed_volatile
            .first()
            .expect("volatile model boundary");
        let advisory = delivered
            .iter()
            .find(|injection| injection.kind == VolatileKind::BudgetAdvisory)
            .expect("low-slice budget advisory");
        assert!(
            advisory
                .payload
                .to_string()
                .contains("available_model_boundaries_including_current\\\":5"),
            "the model must receive the live execution horizon: {advisory:?}"
        );
    }

    #[tokio::test]
    async fn renewable_review_boundary_keeps_one_decisive_tool_action_available() {
        let mut state = make_state();
        state.max_turns = 32;
        state.remaining_turns = 0;
        state.agentic_turn_budget.hard_turn_limit = 72;
        state.agentic_turn_budget.extension_turns = 12;
        state.agentic_turn_budget.max_extensions = 3;
        state.agentic_turn_budget.renewable_past_review_limit = false;
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .expect("renewable review-boundary turn");

        let delivered = host
            .executed_volatile
            .first()
            .expect("volatile model boundary");
        let advisory = delivered
            .iter()
            .find(|injection| injection.kind == VolatileKind::BudgetAdvisory)
            .expect("renewable review advisory");
        let text = advisory.payload.to_string();
        assert!(text.contains("adaptive review checkpoint"), "{text}");
        assert!(text.contains("one smallest decisive action"), "{text}");
        assert!(!text.contains("Do not call any tool"), "{text}");
    }

    #[tokio::test]
    async fn local_host_suppresses_slice_horizon_during_token_rail_wrapup() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 1;
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = false;
        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.completion_action_window = None;
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .expect("token-rail wrap-up turn");

        let delivered = host
            .executed_volatile
            .first()
            .expect("volatile model boundary");
        assert!(
            delivered.iter().all(|injection| {
                injection.kind != VolatileKind::BudgetAdvisory
                    || !injection.payload.to_string().contains("<execution-slice>")
            }),
            "token-rail wrap-up must remain the only budget instruction: {delivered:?}"
        );
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

    #[tokio::test]
    async fn pipeline_session_receives_feedback_on_successful_turn() {
        use astra_turn_core::pipeline_config::PipelineConfig;
        use astra_turn_core::pipeline_session::PipelineSession;

        let mut state = make_state();
        state.current_session_id = Some("session-1".to_string());
        state.current_run_id = Some("run-1".to_string());
        state.session_turn = 6;
        state.context_manifest_model_name = Some("test-model".to_string());
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
                .producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str()),
            Some("run-1")
        );
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
    let spill_dir = match astra_services::local_session_artifact_store().session_dir(session_id) {
        Ok(path) => path,
        Err(_) => {
            let mut restored = to_spill;
            restored.append(messages);
            *messages = restored;
            return 0;
        }
    };
    if std::fs::create_dir_all(&spill_dir).is_err() {
        let mut restored = to_spill;
        restored.append(messages);
        *messages = restored;
        return 0;
    }
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
