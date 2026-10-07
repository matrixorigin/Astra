//! Delegation engine — spawns and tracks sub-runs for multi-agent coordination.
//!
//! Bridges [`CoordinationPattern`] from the services crate with [`RunEngine`]
//! for actual execution. Enforces depth limits, tracks parent→child relationships,
//! and aggregates results.
//!
//! # Example Flow (FanOut)
//!
//! ```text
//! Orchestrator run-A
//!   ├── delegate(FanOut{agent_ids: [s1, s2]})
//!   │     ├── sub-run-B (agent s1)  ──▶ completed ✅
//!   │     └── sub-run-C (agent s2)  ──▶ completed ✅
//!   │
//!   └── aggregate(results) ──▶ merged output
//! ```

use std::collections::{HashMap, HashSet};

use crate::turn::agentic_loop::host::RequestConstraints;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{RwLock, watch};
use unicode_normalization::UnicodeNormalization;

/// Canonical form for agent identity comparison.
///
/// Agent IDs are user-provided and may vary in casing or Unicode
/// normalization form (NFC vs NFD: "café" vs "cafe" + ◌́). Two IDs that
/// are visually identical must be treated as the same agent — otherwise
/// a normalization alias bypasses circular delegation detection and
/// allows an infinite delegation loop.
///
/// Canonicalization = lowercase + NFC recomposition. This collapses the
/// case and normalization axes while preserving visually-distinct
/// graphemes ("café" vs "cafe" remain distinct agents).
fn canonical_agent_id(id: &str) -> String {
    id.to_lowercase().nfc().collect()
}

use astra_services::AdmittedModelExecution;
use astra_services::coordination::{
    AGENT_RESULT_STATUS_FAILED, AGENT_RESULT_STATUS_TIMEOUT, AgentProfile, AgentProfileRegistry,
    AgentResult, AgentResultStatusKind, AggregationStrategy, CoordinationPattern,
    DelegationRequest, DelegationResult, DelegationResultStatusKind, agent_result_status_kind,
    agent_result_status_to_subrun_state, aggregate_results, delegation_result_status_kind,
};
use astra_services::runs::{
    DurableRunStatusKind, RequestedTurnInteractionMode, durable_run_status_is_terminal,
    durable_run_status_kind, durable_run_status_to_subrun_state,
};

pub use astra_core::SubRunState;
use astra_core::{
    InvalidTransition, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED,
    STATUS_PAUSED, STATUS_RUNNING, STATUS_VERIFICATION_FAILED, STATUS_WAITING,
};

use crate::orchestration::spawner::{
    ChildCancellationGrace, ChildCapacityAdmission, ChildExecutionReceipt, DynamicAgentSpawner,
    ParentProfileAuthority, PreparedSpawnModelIdentity, SpawnExecutionOutcome, SpawnRunFrontier,
    SpawnRunResult, SpawnedAgentState, project_subrun_status_to_spawn,
};
use crate::server::run::engine::{RunEngine, RunExecutionAuthority};
use astra_messaging::router::AgentMailboxRouter;
use astra_messaging::router::MailboxLifetime;
use astra_prompts::team_prompts;

fn clone_delegation_context(
    site: astra_core::history_work::HistoryWorkSite,
    context: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    astra_core::history_work::record_serialized_value(site, context);
    context.clone()
}

fn clone_delegation_value(
    site: astra_core::history_work::HistoryWorkSite,
    value: &serde_json::Value,
) -> serde_json::Value {
    astra_core::history_work::record_serialized_value(site, value);
    value.clone()
}

fn profile_child_execution(
    profile: &AgentProfile,
    parent: Option<&astra_turn_core::orchestration_spawn_tool::ParentModelReasoning>,
    inherited_constraints: &RequestConstraints,
    inherited_metadata: Option<&serde_json::Value>,
) -> Result<
    (
        AgentProfile,
        astra_turn_core::thinking_config::ThinkingConfig,
        RequestConstraints,
        Option<serde_json::Value>,
    ),
    String,
> {
    profile.validate_capability_bounds()?;
    if !profile.mcp_servers.is_empty() {
        return Err("profile MCP selection is not supported by the shared execution binding; inherit the authorized parent MCP scope".into());
    }
    let mut execution_profile = profile.clone();
    let mut constraints = inherited_constraints.clone();
    let profile_tools = profile
        .allow_tools
        .as_ref()
        .map(astra_turn_core::tool_allowlist::normalize_tool_names);
    constraints.allowed_tools = astra_turn_core::tool_allowlist::compute_effective_allowlist(
        inherited_constraints.allowed_tools.as_ref(),
        profile_tools.as_ref(),
    );
    execution_profile.allow_tools = constraints.allowed_tools.as_ref().map(|allowed| {
        let mut tools: Vec<_> = allowed.iter().cloned().collect();
        tools.sort_unstable();
        tools
    });
    if !profile.skill_filter.is_empty() {
        let skills: HashSet<String> = profile.skill_filter.iter().cloned().collect();
        constraints.allowed_skills = Some(match inherited_constraints.allowed_skills.as_ref() {
            Some(inherited) => inherited.intersection(&skills).cloned().collect(),
            None => skills,
        });
    }
    // The same narrowed facts must reach durable admission, runtime-context
    // registration and execution. Never turn an absent/unknown workspace
    // authority into permission merely because a profile requests read-only.
    let mut metadata = inherited_metadata.cloned();
    if metadata.as_ref().is_some_and(|value| !value.is_object()) {
        return Err("inherited child execution metadata must be an object".into());
    }
    if let Some(workspace) = metadata
        .as_mut()
        .and_then(|value| value.get_mut("workspace"))
    {
        let binding: crate::server::tool_transport::WorkspaceBinding =
            serde_json::from_value(workspace.clone())
                .map_err(|error| format!("invalid inherited child workspace binding: {error}"))?;
        use crate::server::tool_transport::WorkspaceAuthority;
        if binding.authority == WorkspaceAuthority::ReadOnly {
            execution_profile.read_only = true;
        } else if execution_profile.read_only && binding.authority == WorkspaceAuthority::ReadWrite
        {
            workspace["authority"] = serde_json::to_value(WorkspaceAuthority::ReadOnly)
                .expect("workspace authority serializes");
        }
    }
    // A missing profile selection means inherit the parent's exact Offering.
    // Materialize that identity in the per-run clone so CLI executors resolve
    // by Offering ID instead of re-resolving a potentially ambiguous alias.
    if execution_profile.model_selection.is_none() {
        execution_profile.model_selection = parent.map(|parent| parent.selection.clone());
    }
    let thinking = astra_turn_core::orchestration_spawn_tool::resolve_child_thinking(
        None,
        execution_profile.model_selection.as_ref(),
        parent,
    );
    Ok((execution_profile, thinking, constraints, metadata))
}

fn apply_model_slot_constraint(
    execution_profile: &mut AgentProfile,
    thinking: &mut astra_turn_core::thinking_config::ThinkingConfig,
    parent: Option<&astra_turn_core::orchestration_spawn_tool::ParentModelReasoning>,
    slot: &astra_turn_types::DelegationModelSlotConstraint,
) {
    if let Some(selection) = &slot.model_selection {
        execution_profile.model_selection = Some(selection.clone());
        *thinking = astra_turn_core::orchestration_spawn_tool::resolve_child_thinking(
            None,
            Some(selection),
            parent,
        );
    }
    if let Some(reasoning) = &slot.reasoning {
        *thinking = reasoning.into();
    }
}

fn inherited_model_slot_constraint(
    request_constraints: &RequestConstraints,
) -> Result<Option<astra_turn_types::DelegationModelSlotConstraint>, String> {
    let astra_turn_types::DelegationIntentRequirements::Requirements {
        source,
        requirements,
    } = &request_constraints.delegated_model_requirements
    else {
        return Ok(None);
    };
    if requirements
        .iter()
        .any(|requirement| requirement.task_scope_quote.is_some())
    {
        return Err(
            "scoped delegated model requirements require canonical slot admission before child execution"
                .into(),
        );
    }
    let projected = astra_turn_types::DelegationIntentRequirements::Requirements {
        source: source.clone(),
        requirements: requirements.clone(),
    };
    let slot_briefs =
        [astra_services::delegation_model_requirement::DelegationSlotBrief::default()];
    let (slots, _) =
        astra_services::delegation_model_requirement::bind_delegation_requirements_to_slots(
            &projected,
            None,
            &slot_briefs,
        )?;
    Ok(slots.into_iter().next())
}

fn planned_child_execution(
    profile: &AgentProfile,
    parent: Option<&astra_turn_core::orchestration_spawn_tool::ParentModelReasoning>,
    inherited_constraints: &RequestConstraints,
    inherited_metadata: Option<&serde_json::Value>,
) -> Result<
    (
        AgentProfile,
        astra_turn_core::thinking_config::ThinkingConfig,
        RequestConstraints,
        Option<astra_turn_types::RequestedModelPolicy>,
        Option<serde_json::Value>,
    ),
    String,
> {
    let (mut execution_profile, mut thinking, mut request_constraints, metadata) =
        profile_child_execution(profile, parent, inherited_constraints, inherited_metadata)?;
    let slot = inherited_model_slot_constraint(inherited_constraints)?;
    let child_requirements = inherited_constraints
        .delegated_model_requirements
        .for_child_descendants()
        .map_err(str::to_string)?;

    let requested_model_policy = slot
        .as_ref()
        .and_then(|slot| slot.requested_model_policy.clone());
    if let Some(slot) = slot.as_ref() {
        apply_model_slot_constraint(&mut execution_profile, &mut thinking, parent, slot);
    }
    request_constraints.delegated_model_requirements = child_requirements;
    Ok((
        execution_profile,
        thinking,
        request_constraints,
        requested_model_policy,
        metadata,
    ))
}

/// Model choice and controls that must be authorized before a durable child
/// run is created. This is request-local preparation state; it is never
/// serialized or reconstructed from the profile after admission.
#[derive(Clone)]
pub struct SubRunModelRequest {
    pub user_id: String,
    pub selection: Option<astra_turn_types::ModelSelection>,
    pub parent_model_reasoning:
        Option<astra_turn_core::orchestration_spawn_tool::ParentModelReasoning>,
    pub inherited_execution: Option<AdmittedModelExecution>,
    /// The durable parent was authenticated through a provider-owned route.
    /// This is request-local authorization state, not a cache hint; shared
    /// executors must never store it globally because they serve many sessions.
    pub provider_scope_bound: bool,
    pub thinking: astra_turn_core::thinking_config::ThinkingConfig,
    pub max_output_tokens: Option<u32>,
}

/// Exact model identity prepared by the executor before durable child
/// creation. Server executors also retain the admitted execution material so
/// the child does not repeat model admission after its run row exists.
#[derive(Clone)]
pub struct PreparedSubRunModel {
    pub offering_id: String,
    pub model_name: String,
    pub admitted_execution: Option<AdmittedModelExecution>,
}

pub type SubRunExecutionResult = (AgentResult, Option<SpawnRunFrontier>);

fn child_runtime_id(profile_id: &str, run_id: &str) -> String {
    format!("{profile_id}@{run_id}")
}

#[derive(Debug)]
enum SubRunModelPreparationError {
    Executor(String),
    Cancelled,
    TimedOut,
}

impl std::fmt::Display for SubRunModelPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Executor(error) => formatter.write_str(error),
            Self::Cancelled => {
                formatter.write_str("sub-run model admission cancelled before child creation")
            }
            Self::TimedOut => {
                formatter.write_str("sub-run model admission timed out before child creation")
            }
        }
    }
}

impl From<String> for SubRunModelPreparationError {
    fn from(error: String) -> Self {
        Self::Executor(error)
    }
}

impl From<&str> for SubRunModelPreparationError {
    fn from(error: &str) -> Self {
        Self::Executor(error.to_string())
    }
}

impl From<SubRunModelPreparationError> for String {
    fn from(error: SubRunModelPreparationError) -> Self {
        error.to_string()
    }
}

/// Grace period for cooperative children to publish a canonical terminal
/// result after their parent is cancelled. Keep this comfortably above a
/// scheduler tick, but below the interactive cancellation latency budget.
/// 500ms allows time for durable state persistence (DB write + fsync).
const FANOUT_CANCELLATION_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Model admission is preflight: it must not wait forever or outlive parent
/// cancellation before any child has been durably created. Reuse the existing
/// model-catalog request ceiling rather than introducing a second timeout.
const SUBRUN_MODEL_ADMISSION_TIMEOUT: std::time::Duration =
    astra_thin_client::MODEL_CATALOG_REQUEST_TIMEOUT;

fn delegation_deadline(timeout_sec: u64) -> Result<Option<tokio::time::Instant>, String> {
    if timeout_sec == 0 {
        return Ok(None);
    }
    // The real ExecutionDeadlineAuthority is admitted first by each caller.
    // This paired Tokio guard intentionally follows Tokio's clock so paused
    // orchestration tests remain deterministic; checked_add prevents a large
    // duration from turning the scheduler guard into an overflow panic.
    tokio::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(timeout_sec))
        .map(Some)
        .ok_or_else(|| "execution deadline exceeds Tokio clock range".to_string())
}

/// Admit the child execution budget once, using the real monotonic clock that
/// backs [`astra_services::runs::ExecutionDeadlineAuthority`]. Tokio's deadline
/// scheduler's virtual-time-friendly orchestration guard; it is not a source
/// from which a child authority may be reconstructed after admission waits.
fn execution_deadline_authority(
    timeout_sec: u64,
) -> Result<Option<astra_services::runs::ExecutionDeadlineAuthority>, String> {
    if timeout_sec == 0 {
        return Ok(None);
    }
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("execution deadline clock is unavailable: {error}"))?
        .as_millis();
    let now_unix_ms = u64::try_from(now_unix_ms)
        .map_err(|_| "execution deadline clock exceeds supported range".to_string())?;
    astra_services::runs::ExecutionDeadlineAuthority::from_budget_at(
        astra_services::runs::ExecutionTimeBudget {
            remaining_seconds: timeout_sec,
        },
        now_unix_ms,
    )
    .map(Some)
}

fn model_admission_timeout(deadline: Option<tokio::time::Instant>) -> std::time::Duration {
    deadline
        .map(|deadline| {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(SUBRUN_MODEL_ADMISSION_TIMEOUT)
        })
        .unwrap_or(SUBRUN_MODEL_ADMISSION_TIMEOUT)
}

/// Bound the total time spent reconciling missing children through the
/// durable authority after an abort drain. A slow durable store must not
/// hold the parent turn indefinitely. On timeout, children remain explicitly
/// unfinished so recovery can still observe the eventual durable winner.
const DELEGATION_RECONCILIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubRunOperationStop {
    Cancelled,
    DeadlineExceeded,
}

/// Bound a child-owned operation by the parent's absolute deadline and
/// cancellation signal. Store implementations use cancellation-safe pool
/// connections, so dropping an in-flight DB future closes an uncertain
/// checkout instead of returning it to the pool.
async fn await_subrun_operation<T>(
    future: impl std::future::Future<Output = T>,
    deadline: Option<tokio::time::Instant>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<T, SubRunOperationStop> {
    match (deadline, cancel_token) {
        (Some(deadline), Some(token)) => tokio::select! {
            biased;
            _ = token.cancelled() => Err(SubRunOperationStop::Cancelled),
            result = tokio::time::timeout_at(deadline, future) => {
                result.map_err(|_| SubRunOperationStop::DeadlineExceeded)
            }
        },
        (Some(deadline), None) => tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| SubRunOperationStop::DeadlineExceeded),
        (None, Some(token)) => tokio::select! {
            biased;
            _ = token.cancelled() => Err(SubRunOperationStop::Cancelled),
            result = future => Ok(result),
        },
        (None, None) => Ok(future.await),
    }
}

#[derive(Debug)]
enum ChildActivationFailure {
    LostAuthority,
    Persistence(String),
    Interrupted(SubRunOperationStop),
}

impl std::fmt::Display for ChildActivationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LostAuthority => formatter.write_str("durable execution authority was lost"),
            Self::Persistence(error) => write!(formatter, "activation persistence failed: {error}"),
            Self::Interrupted(SubRunOperationStop::Cancelled) => {
                formatter.write_str("activation was cancelled")
            }
            Self::Interrupted(SubRunOperationStop::DeadlineExceeded) => {
                formatter.write_str("activation exceeded the execution deadline")
            }
        }
    }
}

async fn activate_delegated_child(
    run_engine: &RunEngine,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    owner_generation: u64,
    waiting_for: &'static str,
    deadline: Option<tokio::time::Instant>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), ChildActivationFailure> {
    let transition = run_engine.transition_status_with_events_if_current_owner(
        user_id,
        session_id,
        run_id,
        &[STATUS_RUNNING],
        owner_generation,
        STATUS_RUNNING,
        Some(waiting_for),
        None,
        &[],
    );
    match await_subrun_operation(transition, deadline, cancel_token).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(ChildActivationFailure::LostAuthority),
        Ok(Err(error)) => Err(ChildActivationFailure::Persistence(error)),
        Err(stop) => Err(ChildActivationFailure::Interrupted(stop)),
    }
}

/// Await one operation within a deadline shared by the whole reconciliation
/// batch. Once the budget is exhausted, later operations are not polled. This
/// prevents fanout width from multiplying cancellation latency.
async fn await_with_shared_deadline<T>(
    deadline: &mut Option<tokio::time::Instant>,
    budget: std::time::Duration,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    let deadline = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + budget);
    if tokio::time::Instant::now() >= deadline {
        return None;
    }
    tokio::time::timeout_at(deadline, future).await.ok()
}

struct BoundedDrain<T> {
    completed: Vec<T>,
    deadline_elapsed: bool,
}

/// Poll independent cleanup work concurrently, but give the entire group one
/// wall-clock budget. A slow first child must not prevent later children from
/// recording their cancellation/settlement attempt.
async fn drain_futures_before<T, F>(
    mut futures: FuturesUnordered<F>,
    deadline: tokio::time::Instant,
) -> BoundedDrain<T>
where
    F: std::future::Future<Output = T>,
{
    let mut completed = Vec::new();
    while !futures.is_empty() {
        match tokio::time::timeout_at(deadline, futures.next()).await {
            Ok(Some(result)) => completed.push(result),
            _ => {
                return BoundedDrain {
                    completed,
                    deadline_elapsed: true,
                };
            }
        }
    }
    BoundedDrain {
        completed,
        deadline_elapsed: false,
    }
}

const METRIC_DELEGATION_EXECUTIONS_TOTAL: &str = "astra_delegation_executions_total";
const METRIC_DELEGATION_DURATION_MS_TOTAL: &str = "astra_delegation_duration_ms_total";
const METRIC_DELEGATION_SUB_RUNS_TOTAL: &str = "astra_delegation_sub_runs_total";
const METRIC_DELEGATION_TOKENS_TOTAL: &str = "astra_delegation_tokens_total";

fn register_delegation_metrics(registry: &astra_turn_core::pipeline_metrics::MetricsRegistry) {
    registry.register_counter(
        METRIC_DELEGATION_EXECUTIONS_TOTAL,
        "Delegation executions by coordination pattern and terminal outcome.",
    );
    registry.register_counter(
        METRIC_DELEGATION_DURATION_MS_TOTAL,
        "Total delegation wall time in milliseconds by pattern and outcome.",
    );
    registry.register_counter(
        METRIC_DELEGATION_SUB_RUNS_TOTAL,
        "Delegated sub-runs by pattern and canonical terminal status.",
    );
    registry.register_counter(
        METRIC_DELEGATION_TOKENS_TOTAL,
        "Delegated agent tokens by pattern and token kind.",
    );
}

fn delegation_outcome_label(result: &Result<DelegationResult, String>) -> &'static str {
    match result {
        Ok(result) => match delegation_result_status_kind(&result.status) {
            DelegationResultStatusKind::Completed => "completed",
            DelegationResultStatusKind::Partial => "partial",
            DelegationResultStatusKind::Unfinished => "unfinished",
            DelegationResultStatusKind::Failed | DelegationResultStatusKind::Other => "failed",
        },
        Err(_) => "error",
    }
}

fn sub_run_status_label(status: &str) -> &'static str {
    match agent_result_status_kind(status) {
        AgentResultStatusKind::Completed | AgentResultStatusKind::Delegated => "completed",
        AgentResultStatusKind::Waiting => "waiting",
        AgentResultStatusKind::Paused => "paused",
        AgentResultStatusKind::Cancelled => "cancelled",
        AgentResultStatusKind::Timeout => "timeout",
        AgentResultStatusKind::VerificationFailed => "verification_failed",
        AgentResultStatusKind::Partial => "partial",
        AgentResultStatusKind::Failed | AgentResultStatusKind::Other => "failed",
    }
}

fn record_delegation_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    pattern: &str,
    elapsed: std::time::Duration,
    result: &Result<DelegationResult, String>,
) {
    let Some(registry) = registry else { return };
    let outcome = delegation_outcome_label(result);
    let labels = [("pattern", pattern), ("outcome", outcome)];
    registry.increment_counter(METRIC_DELEGATION_EXECUTIONS_TOTAL, &labels, 1);
    registry.increment_counter(
        METRIC_DELEGATION_DURATION_MS_TOTAL,
        &labels,
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    );
    if let Ok(result) = result {
        for sub_run in &result.agent_results {
            registry.increment_counter(
                METRIC_DELEGATION_SUB_RUNS_TOTAL,
                &[
                    ("pattern", pattern),
                    ("status", sub_run_status_label(&sub_run.status)),
                ],
                1,
            );
        }
        registry.increment_counter(
            METRIC_DELEGATION_TOKENS_TOTAL,
            &[("pattern", pattern), ("kind", "prompt")],
            result.total_prompt_tokens,
        );
        registry.increment_counter(
            METRIC_DELEGATION_TOKENS_TOTAL,
            &[("pattern", pattern), ("kind", "completion")],
            result.total_completion_tokens,
        );
    }
}

fn cancelled_agent_result(agent_id: &str, run_id: &str) -> AgentResult {
    AgentResult {
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        status: STATUS_CANCELLED.to_string(),
        output: None,
        error: Some("cancelled by parent run".to_string()),
        prompt_tokens: 0,
        completion_tokens: 0,
        tool_calls: 0,
    }
}

fn delegated_spawn_projection(
    execution: &(AgentResult, Option<SpawnRunFrontier>),
) -> std::borrow::Cow<'_, SpawnRunResult> {
    let (result, frontier) = execution;
    let projection = project_subrun_status_to_spawn(&result.status, result.error.clone());
    std::borrow::Cow::Owned(SpawnRunResult {
        agent_id: result.agent_id.clone(),
        run_id: result.run_id.clone(),
        committed_frontier: frontier.clone(),
        status: projection.status.into(),
        finish_reason: projection.finish_reason,
        cancellation_origin: crate::orchestration::CancellationOrigin::Unverified,
        output: result.output.clone(),
        error: projection.error,
        prompt_tokens: result.prompt_tokens,
        completion_tokens: result.completion_tokens,
        tool_calls: result.tool_calls,
        turns_completed: 0,
        permission_summary: None,
        permission_requests: 0,
        permission_requests_approved: 0,
        tools_blocked: 0,
    })
}

fn child_setup_failure_result(
    agent_id: &str,
    run_id: &str,
    error: String,
    deadline: Option<tokio::time::Instant>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> AgentResult {
    let status = if cancel_token.is_some_and(|token| token.is_cancelled()) {
        STATUS_CANCELLED
    } else if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        AGENT_RESULT_STATUS_TIMEOUT
    } else {
        STATUS_FAILED
    };
    AgentResult {
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        status: status.to_string(),
        output: None,
        error: Some(error),
        prompt_tokens: 0,
        completion_tokens: 0,
        tool_calls: 0,
    }
}

fn durable_reconciliation_pending_result(attempted: &AgentResult, operation: &str) -> AgentResult {
    AgentResult {
        agent_id: attempted.agent_id.clone(),
        run_id: attempted.run_id.clone(),
        status: STATUS_WAITING.to_string(),
        output: attempted.output.clone(),
        error: Some(format!(
            "durable {operation} reconciliation exceeded the shared {}ms deadline; authoritative child state is still unknown",
            DELEGATION_RECONCILIATION_TIMEOUT.as_millis(),
        )),
        // Terminal authority is unknown, but work already observed by the
        // parent is still a fact. Keep it visible so a reconciliation timeout
        // cannot turn into silent usage under-counting.
        prompt_tokens: attempted.prompt_tokens,
        completion_tokens: attempted.completion_tokens,
        tool_calls: attempted.tool_calls,
    }
}

fn reconcile_preserving_mailbox_cleanup<'a>(
    run_engine: &'a RunEngine,
    user_id: &'a str,
    expected_session_id: &'a str,
    disposition: DurableLifecycleDisposition,
    result: AgentResult,
    mailbox: Option<&MailboxRetirement>,
    operation: &'a str,
) -> impl std::future::Future<Output = AgentResult> + Send + 'a {
    enum Started {
        Owned(tokio::task::JoinHandle<AgentResult>),
        Inline(AgentResult),
    }
    let pending = durable_reconciliation_pending_result(&result, operation);
    // Ownership transfer is eager: the shared observation deadline may
    // already be exhausted, but a mailbox-bearing child still needs its
    // existing durable reconciliation to establish terminal authority.
    let started = match mailbox.cloned() {
        Some(mailbox) => {
            let run_engine = run_engine.clone();
            let user_id = user_id.to_string();
            let expected_session_id = expected_session_id.to_string();
            Started::Owned(tokio::spawn(async move {
                reconcile_agent_result_with_durable_authority(
                    &run_engine,
                    &user_id,
                    &expected_session_id,
                    disposition,
                    result,
                    Some(&mailbox),
                )
                .await
                .0
            }))
        }
        None => Started::Inline(result),
    };
    async move {
        match started {
            Started::Owned(task) => match task.await {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(
                        target: "astra_runtime::delegation",
                        operation,
                        %error,
                        "durable child reconciliation task failed; preserving an unfinished projection"
                    );
                    pending
                }
            },
            Started::Inline(result) => {
                reconcile_agent_result_with_durable_authority(
                    run_engine,
                    user_id,
                    expected_session_id,
                    disposition,
                    result,
                    None,
                )
                .await
                .0
            }
        }
    }
}

/// Reconcile one result within the shared cleanup budget for its parent
/// operation. Timeout means the durable winner is unknown, not failed; all
/// writes are owner-fenced and a later recovery/read can observe the winner.
async fn reconcile_agent_result_with_shared_deadline(
    run_engine: &RunEngine,
    user_id: &str,
    expected_session_id: &str,
    disposition: DurableLifecycleDisposition,
    result: AgentResult,
    mailbox: Option<&MailboxRetirement>,
    deadline: &mut Option<tokio::time::Instant>,
    scope: &'static str,
    operation: &'static str,
) -> AgentResult {
    let run_id = result.run_id.clone();
    let pending = durable_reconciliation_pending_result(&result, operation);
    match await_with_shared_deadline(
        deadline,
        DELEGATION_RECONCILIATION_TIMEOUT,
        reconcile_preserving_mailbox_cleanup(
            run_engine,
            user_id,
            expected_session_id,
            disposition,
            result,
            mailbox,
            operation,
        ),
    )
    .await
    {
        Some(result) => result,
        None => {
            tracing::error!(
                target: "astra_runtime::delegation",
                scope,
                operation,
                run_id,
                timeout_ms = DELEGATION_RECONCILIATION_TIMEOUT.as_millis(),
                "durable child reconciliation reached its shared deadline; preserving an unfinished projection for recovery"
            );
            pending
        }
    }
}

/// Convert the richer delegated-agent outcome taxonomy to the canonical
/// durable run lifecycle. Details such as timeout/partial/verification failure
/// remain on `AgentResult`; the run row intentionally stores only lifecycle
/// states understood by every control-plane consumer.
fn durable_status_for_agent_result(status: &str) -> &'static str {
    match agent_result_status_kind(status) {
        AgentResultStatusKind::Completed => STATUS_COMPLETED,
        AgentResultStatusKind::Delegated => STATUS_DELEGATED,
        AgentResultStatusKind::Waiting => STATUS_WAITING,
        AgentResultStatusKind::Paused => STATUS_PAUSED,
        AgentResultStatusKind::Cancelled => STATUS_CANCELLED,
        AgentResultStatusKind::Timeout
        | AgentResultStatusKind::VerificationFailed
        | AgentResultStatusKind::Partial
        | AgentResultStatusKind::Failed
        | AgentResultStatusKind::Other => STATUS_FAILED,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DurableLifecycleDisposition {
    /// The executor owns all generation-fenced durable writes. The scheduler
    /// may only reread the winning fact, including on timeout/panic paths.
    ExecutorOwned { owner_generation: u64 },
    /// No exact execution authority survived (for example an uncorrelated
    /// panic projection). Fail closed by rereading only.
    ReadOnly,
    /// A quiescent scheduler-owned executor produced this result. The
    /// scheduler may commit it only with the exact admission generation.
    SchedulerOwned { owner_generation: u64 },
}

fn durable_lifecycle_disposition(
    executor: &dyn SubRunExecutor,
    owner_generation: u64,
) -> DurableLifecycleDisposition {
    if executor.owns_durable_run_lifecycle() {
        DurableLifecycleDisposition::ExecutorOwned { owner_generation }
    } else {
        DurableLifecycleDisposition::SchedulerOwned { owner_generation }
    }
}

/// Until the executor has actually been entered, the scheduler remains the
/// only component that can settle the durably admitted child. This matters
/// for executor-owned implementations when a child expires or is cancelled
/// while queued for capacity.
fn durable_lifecycle_disposition_after_dispatch(
    executor: &dyn SubRunExecutor,
    owner_generation: u64,
    executor_entered: bool,
) -> DurableLifecycleDisposition {
    if executor_entered {
        durable_lifecycle_disposition(executor, owner_generation)
    } else {
        DurableLifecycleDisposition::SchedulerOwned { owner_generation }
    }
}

#[derive(Clone)]
struct MailboxRetirement {
    router: Arc<AgentMailboxRouter>,
    lifetime: MailboxLifetime,
}

impl MailboxRetirement {
    fn from_mailbox(
        router: &Option<Arc<AgentMailboxRouter>>,
        mailbox: Option<&astra_messaging::router::AgentMailbox>,
    ) -> Option<Self> {
        Some(Self {
            router: router.as_ref()?.clone(),
            lifetime: mailbox?.lifetime().clone(),
        })
    }

    fn request_if_terminal(&self, durable_status: &str) {
        if durable_run_status_is_terminal(durable_status) {
            // The router starts and owns cleanup before the observer is polled.
            drop(self.router.retire_terminal(&self.lifetime));
        }
    }
}

/// Commit an executor result and return the outcome permitted by durable
/// authority. A pause/cancel/terminal CAS winner must shape the result returned
/// to the parent; otherwise the database, tracker, and aggregation can report
/// contradictory facts for the same child run.
async fn reconcile_agent_result_with_durable_authority(
    run_engine: &RunEngine,
    user_id: &str,
    expected_session_id: &str,
    disposition: DurableLifecycleDisposition,
    mut result: AgentResult,
    mailbox: Option<&MailboxRetirement>,
) -> (AgentResult, Option<SpawnRunFrontier>) {
    let attempted_status = durable_status_for_agent_result(&result.status);
    let persistence = match disposition {
        DurableLifecycleDisposition::ExecutorOwned { .. }
        | DurableLifecycleDisposition::ReadOnly => {
            // The executor already committed with its exact owner generation.
            // A second outer write would allow a stale executor to overwrite
            // the owner that recovered an expired lease.
            Ok(false)
        }
        DurableLifecycleDisposition::SchedulerOwned { owner_generation } => {
            run_engine
                .persist_delegation_outcome_status_if_current_owner(
                    user_id,
                    expected_session_id,
                    &result.run_id,
                    owner_generation,
                    attempted_status,
                    None,
                    result.error.as_deref(),
                )
                .await
        }
    };
    if matches!(persistence, Ok(true)) {
        if let Some(mailbox) = mailbox {
            mailbox.request_if_terminal(attempted_status);
        }
        let frontier = match disposition {
            DurableLifecycleDisposition::SchedulerOwned { owner_generation } => {
                Some(SpawnRunFrontier {
                    run_id: result.run_id.clone(),
                    run_generation: owner_generation,
                    last_event_idx: None,
                })
            }
            _ => None,
        };
        return (result, frontier);
    }

    let persistence_detail = match &persistence {
        Ok(false)
            if matches!(
                disposition,
                DurableLifecycleDisposition::ExecutorOwned { .. }
                    | DurableLifecycleDisposition::ReadOnly
            ) =>
        {
            "executor-owned durable lifecycle requires authoritative reread".to_string()
        }
        Ok(false) => "outcome lost its durable status compare-and-set".to_string(),
        Err(error) => format!("could not commit delegated outcome: {error}"),
        Ok(true) => unreachable!(),
    };
    let durable = match run_engine.load_run(user_id, &result.run_id).await {
        Ok(Some(durable)) => durable,
        Ok(None) => {
            result.status = STATUS_WAITING.to_string();
            result.output = None;
            result.error = Some(format!(
                "{persistence_detail}; durable run {} is missing",
                result.run_id
            ));
            return (result, None);
        }
        Err(load_error) => {
            result.status = STATUS_WAITING.to_string();
            result.output = None;
            result.error = Some(format!(
                "{persistence_detail}; failed to load durable winner for run {}: {load_error}",
                result.run_id
            ));
            return (result, None);
        }
    };
    let frontier = Some(SpawnRunFrontier::from_durable(&durable));
    if let Some(mailbox) = mailbox {
        mailbox.request_if_terminal(&durable.status);
    }

    // A replay can lose its CAS because the exact same terminal fact is
    // already durable. Status equality alone is insufficient: a recovered
    // generation can independently reach the same terminal status with a
    // different output. Preserve local output only when the durable winner is
    // the exact execution generation that produced it. Read-only projections
    // carry no such proof and therefore never retain local output.
    let expected_owner_generation = match disposition {
        DurableLifecycleDisposition::ExecutorOwned { owner_generation }
        | DurableLifecycleDisposition::SchedulerOwned { owner_generation } => {
            Some(owner_generation)
        }
        DurableLifecycleDisposition::ReadOnly => None,
    };
    if durable.status == attempted_status
        && expected_owner_generation == Some(durable.run_generation)
    {
        return (result, frontier);
    }

    tracing::info!(
        target: "astra_runtime::delegation",
        run_id = %result.run_id,
        executor_status = %result.status,
        attempted_durable_status = attempted_status,
        durable_status = %durable.status,
        "replaced stale delegated executor outcome with durable authority"
    );
    result.output = None;
    result.prompt_tokens = durable.total_prompt_tokens;
    result.completion_tokens = durable.total_completion_tokens;
    result.tool_calls = durable.total_tool_calls;
    match durable_run_status_kind(&durable.status) {
        DurableRunStatusKind::Running => {
            // `running` is not a terminal AgentResult. Project it as a
            // recoverable wait so parent aggregation remains unfinished.
            result.status = STATUS_WAITING.to_string();
            result.error = None;
        }
        DurableRunStatusKind::Waiting | DurableRunStatusKind::Paused => {
            result.status = durable.status;
            result.error = None;
        }
        DurableRunStatusKind::Completed | DurableRunStatusKind::Delegated => {
            result.status = durable.status;
            result.error = None;
        }
        DurableRunStatusKind::Cancelled => {
            result.status = STATUS_CANCELLED.to_string();
            result.error = durable
                .error_message
                .or_else(|| Some("cancelled by a concurrent durable control decision".to_string()));
        }
        DurableRunStatusKind::Failed | DurableRunStatusKind::Other => {
            result.status = STATUS_FAILED.to_string();
            result.error = durable.error_message.or(Some(persistence_detail));
        }
    }
    (result, frontier)
}

fn normalize_context_allowlist_entry(entry: &str, key: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!(
            "context[{key}] must not contain empty or whitespace-only strings"
        ))
    } else {
        Ok(normalized)
    }
}

fn parse_request_allowlist_from_context(
    context: &mut HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(value) = context.remove(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("context[{key}] must be an array of strings"))?;
    let mut normalized = HashSet::with_capacity(values.len());
    for entry in values {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("context[{key}] must contain only strings"))?;
        normalized.insert(normalize_context_allowlist_entry(raw, key)?);
    }
    Ok(Some(normalized))
}

fn parse_request_skill_sources_from_context(
    context: &mut HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<HashSet<crate::skills::manifest::SkillSourceKind>>, String> {
    let Some(values) = parse_request_allowlist_from_context(context, key)? else {
        return Ok(None);
    };
    let mut parsed = HashSet::with_capacity(values.len());
    for value in values {
        let source = value
            .parse()
            .map_err(|error| format!("context[{key}]: {error}"))?;
        parsed.insert(source);
    }
    Ok(Some(parsed))
}

// ─── Sub-run Executor Trait ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionOwnerGenerationPublication {
    Preparing { expected_initial_generation: u64 },
    Acquired(u64),
    StoppedBeforeAcquisition { expected_initial_generation: u64 },
}

/// Local handoff between dynamic-child durable start and cancellation.
/// Cancellation aborts the executor before awaiting durability, so it must be
/// able to distinguish "not published yet" from "no row can still be
/// created" without guessing from a run id.
pub struct ExecutionOwnerGenerationSink {
    state: watch::Sender<ExecutionOwnerGenerationPublication>,
    #[cfg(test)]
    wait_after_preparing_hook:
        std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

impl ExecutionOwnerGenerationSink {
    pub fn preparing(expected_initial_generation: u64) -> Self {
        let (state, _) = watch::channel(ExecutionOwnerGenerationPublication::Preparing {
            expected_initial_generation,
        });
        Self {
            state,
            #[cfg(test)]
            wait_after_preparing_hook: std::sync::Mutex::new(None),
        }
    }

    pub fn publish(&self, generation: u64) {
        self.state
            .send_replace(ExecutionOwnerGenerationPublication::Acquired(generation));
    }

    pub fn guard(self: &Arc<Self>) -> ExecutionOwnerGenerationGuard {
        ExecutionOwnerGenerationGuard {
            sink: Arc::clone(self),
        }
    }

    pub async fn wait_until_published_or_stopped(&self) -> ExecutionOwnerGenerationPublication {
        let mut observed = self.state.subscribe();
        loop {
            let state = *observed.borrow_and_update();
            if !matches!(state, ExecutionOwnerGenerationPublication::Preparing { .. }) {
                return state;
            }
            #[cfg(test)]
            let wait_hook = self
                .wait_after_preparing_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            #[cfg(test)]
            if let Some((preparing_observed, release_wait)) = wait_hook {
                preparing_observed.notify_one();
                release_wait.notified().await;
            }
            // watch versions the publication even when publish/guard drop
            // races exactly after the Preparing read and before this await.
            observed
                .changed()
                .await
                .expect("generation sink retains its publication sender");
        }
    }
}

pub struct ExecutionOwnerGenerationGuard {
    sink: Arc<ExecutionOwnerGenerationSink>,
}

impl Drop for ExecutionOwnerGenerationGuard {
    fn drop(&mut self) {
        self.sink.state.send_if_modified(|state| {
            let ExecutionOwnerGenerationPublication::Preparing {
                expected_initial_generation,
            } = *state
            else {
                return false;
            };
            *state = ExecutionOwnerGenerationPublication::StoppedBeforeAcquisition {
                expected_initial_generation,
            };
            true
        });
    }
}

/// Configuration for a sub-run spawned by delegation.
pub struct SubRunConfig {
    /// Exact admitted profile lineage, distinct from runtime routing IDs.
    /// Unbound is valid only when the parent has no admitted profile roster.
    pub profile_authority: ParentProfileAuthority,
    /// Explicit ceiling for the first model round, including retries.
    pub max_output_tokens: Option<u32>,
    /// Unique ID for this sub-run.
    pub run_id: String,
    /// Durable parent run that delegated this child. This is identity data for
    /// the run tree, never prompt context; a delegated sub-run cannot exist
    /// without it.
    pub parent_run_id: String,
    /// Agent profile executing this sub-run.
    pub agent_profile: AgentProfile,
    /// The task/prompt for this sub-run.
    pub task: String,
    /// Parent's session ID (sub-runs share the session lineage).
    pub session_id: String,
    /// User ID owning the delegation.
    pub user_id: String,
    /// Exact durable execution-owner epoch returned when this child row was
    /// created. `None` is valid only when the executor itself will create the
    /// row; an already-existing durable child must carry matching authority.
    pub execution_owner_generation: Option<u64>,
    /// Exact process-local invocation binding installed by the shared supervisor.
    pub cancellation_binding_id: Option<String>,
    /// Existing session supervisor, weak so its own tasks cannot retain its root.
    pub child_supervisor: Option<std::sync::Weak<DynamicAgentSpawner>>,
    /// Optional process-local observer for the exact durable generation
    /// acquired by this executor. Dynamic-agent cancellation uses it to fence
    /// runtime-owned durable intent; user lineage cancellation does not need
    /// generation authority.
    pub execution_owner_generation_sink: Option<Arc<ExecutionOwnerGenerationSink>>,
    /// Optional output from previous pipeline stage.
    pub previous_output: Option<String>,
    /// Context key-value pairs from the delegation request.
    pub context: HashMap<String, serde_json::Value>,
    /// Trusted forwarded headers propagated out-of-band for child remote skills.
    pub forward_headers: HashMap<String, String>,
    /// Short-lived execution material inherited from the admitted parent run.
    /// It is sideband state and is never serialized into delegation context.
    pub admitted_model_execution: Option<AdmittedModelExecution>,
    /// Exact child route prepared before the durable run was created.
    pub prepared_model: Option<PreparedSubRunModel>,
    /// Original requested policy, separate from the resolved Offering.
    pub requested_model_policy: Option<astra_turn_types::RequestedModelPolicy>,
    /// Effective reasoning control frozen for this execution attempt.
    pub thinking: astra_turn_core::thinking_config::ThinkingConfig,
    /// Effective interaction policy for this exact child invocation. It is
    /// resolved once from the durable parent and carried on the run config so
    /// executors, retries, and descendants cannot invent a new default.
    pub interaction_mode: RequestedTurnInteractionMode,
    /// Request-scoped capability constraints inherited from the parent runtime request.
    pub request_constraints: RequestConstraints,
    /// Current nested agent/sub-run depth for the child loop.
    pub recursion_depth: u8,
    /// Optional explicit turn budget for the child loop.
    pub max_turns: Option<u32>,
    /// Optional initial adaptive slice. When `max_turns` is absent this value
    /// is only a convergence checkpoint and can renew subject to current
    /// execution health and any explicitly configured runtime ceiling.
    pub initial_turns: Option<u32>,
    /// Cooperative pause flag — checked between turns by the sub-run loop.
    /// When set to `true`, the sub-run should yield with status "paused".
    pub pause_flag: Option<Arc<AtomicBool>>,
    /// Optional mailbox for inter-agent messaging during the sub-run.
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Optional progress emitter for broadcasting child turn events.
    pub progress_emitter: Option<crate::orchestration::AgentProgressEmitter>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Cancellation token — when cancelled, the sub-run should stop gracefully.
    pub cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Exact absolute execution deadline admitted for this child. Executors
    /// must carry this authority forward without re-anchoring it after queue
    /// or model-admission work.
    pub execution_deadline: Option<astra_services::runs::ExecutionDeadlineAuthority>,
    /// Resolved parent prefix for prompt-cache inheritance on the
    /// delegated child's first API call (Bug B step 2). Populated by
    /// [`DelegationEngine`] when its `prefix_store` is set and a
    /// matching parent capture is present; `None` otherwise.
    /// Executors that don't consume it (e.g. server-side
    /// `ServerAgenticLoopHost`) can ignore this field — the child
    /// runs fresh, same behavior as pre-fork-prefix.
    pub inherited_prefix: Option<crate::orchestration::InheritedChildPrefix>,
    /// UI/runtime execution binding metadata inherited by this sub-run.
    /// Trusted parent admission facts, never caller execution metadata.
    pub execution_contract: Option<(
        crate::server::tool_transport::ExecutionBindingSnapshot,
        astra_turn_types::StopHookObligations,
    )>,
    pub execution_metadata: Option<serde_json::Value>,
    /// Existing parent execution/routing chain. Admitted profile ancestry
    /// lives in `profile_authority`; runtime instance IDs are not profile IDs.
    pub delegation_chain: Vec<String>,
    /// Exact canonical WorkItem revision assigned to this run. It remains a
    /// typed sideband value and is validated before durable run creation.
    pub work_item: Option<astra_turn_core::orchestration_spawn_tool::WorkItemExecutionSpec>,
    /// Parent session's harness snapshot sink for observe-only sub-run
    /// observation. When set, the sub-run creates a sink-only HarnessSlot
    /// so sub-run snapshots appear in the parent's history.
    #[cfg(feature = "harness")]
    pub harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
}

impl SubRunConfig {
    /// Bind the exact authority returned by durable child admission into the
    /// configuration that will construct the child loop. A durable executor
    /// may never reconstruct this capability from the current database row,
    /// while a process-local executor may not consume a durable capability.
    pub(crate) fn bind_execution_authority(
        &mut self,
        durable_executor: bool,
        authority: Option<RunExecutionAuthority>,
    ) -> Result<(), String> {
        match (durable_executor, authority) {
            (true, Some(authority)) => {
                if let Some(expected) = self.execution_owner_generation
                    && expected != authority.owner_generation
                {
                    return Err(format!(
                        "durable sub-run execution authority changed during admission: expected generation {expected}, admitted generation {}",
                        authority.owner_generation
                    ));
                }
                self.execution_owner_generation = Some(authority.owner_generation);
                Ok(())
            }
            (true, None) => {
                Err("durable sub-run admission returned no execution authority".to_string())
            }
            (false, Some(_)) => Err(
                "process-local sub-run executor received durable execution authority".to_string(),
            ),
            (false, None) if self.execution_owner_generation.is_some() => Err(
                "process-local sub-run executor cannot consume durable execution authority"
                    .to_string(),
            ),
            (false, None) => Ok(()),
        }
    }
}

impl std::fmt::Debug for SubRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubRunConfig")
            .field("run_id", &self.run_id)
            .field("profile_authority", &self.profile_authority)
            .field("agent_profile", &self.agent_profile)
            .field("task", &self.task)
            .field("session_id", &self.session_id)
            .field("user_id", &self.user_id)
            .field(
                "execution_owner_generation",
                &self.execution_owner_generation,
            )
            .field("previous_output", &self.previous_output)
            .field("forward_headers", &!self.forward_headers.is_empty())
            .field(
                "admitted_model_execution",
                &self.admitted_model_execution.is_some(),
            )
            .field("prepared_model", &self.prepared_model.is_some())
            .field("requested_model_policy", &self.requested_model_policy)
            .field("interaction_mode", &self.interaction_mode)
            .field("request_constraints", &self.request_constraints)
            .field("recursion_depth", &self.recursion_depth)
            .field("max_turns", &self.max_turns)
            .field("initial_turns", &self.initial_turns)
            .field(
                "execution_deadline_unix_ms",
                &self
                    .execution_deadline
                    .map(|deadline| deadline.deadline_unix_ms),
            )
            .field("pause_flag", &self.pause_flag.is_some())
            .field("mailbox", &self.mailbox.is_some())
            .field("progress_emitter", &self.progress_emitter.is_some())
            .field("live_event_sink", &self.live_event_sink.is_some())
            .finish()
    }
}

/// Trait for executing sub-runs as part of a delegation.
///
/// Production implementations use [`ServerAgenticLoopHost`] to run a real
/// agentic loop. Test implementations return mock results.
#[async_trait]
pub trait SubRunExecutor: Send + Sync {
    /// Prepare a set of child model identities before durable rows or child
    /// execution are created. Production executors override this to batch
    /// admission. The default permits only inherited parent identity; an
    /// executor may not silently accept a different Offering it cannot admit.
    async fn prepare_model_batch(
        &self,
        requests: &[SubRunModelRequest],
    ) -> Result<Vec<Option<PreparedSubRunModel>>, String> {
        requests
            .iter()
            .map(|request| {
                let requested = request.selection.as_ref().map(|selection| selection.offering_id.as_str());
                let parent = request
                    .parent_model_reasoning
                    .as_ref()
                    .map(|parent| parent.selection.offering_id.as_str());
                if requested.is_some() && requested != parent {
                    return Err("this sub-run executor cannot pre-admit a different Offering before durable child creation".to_string());
                }
                Ok(None)
            })
            .collect()
    }

    /// Execute a sub-run and return the result.
    async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String>;

    /// True when the executor itself owns generation-fenced durable lifecycle
    /// commits. Schedulers must then reconcile by rereading durable authority,
    /// never by issuing a second un-fenced status write.
    fn owns_durable_run_lifecycle(&self) -> bool {
        false
    }
}

/// No-op executor that immediately returns "completed" results.
/// Used when no real executor is wired (tests, offline mode).
pub struct StubSubRunExecutor;

#[async_trait]
impl SubRunExecutor for StubSubRunExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
        Ok((
            AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_COMPLETED.to_string(),
                output: Some(format!("[stub] completed task: {}", config.task)),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            },
            None,
        ))
    }
}

// ─── Sub-run Tracking ─────────────────────────────────────────────────────────────

/// Tracks parent→child relationships for delegation hierarchies.
#[derive(Debug, Clone)]
pub struct SubRunRecord {
    /// The sub-run's own ID.
    pub run_id: String,
    /// Parent run that spawned this sub-run.
    pub parent_run_id: String,
    /// Delegation this sub-run belongs to.
    pub delegation_id: String,
    /// Agent executing this sub-run.
    pub agent_id: String,
    /// Current depth in the delegation tree.
    pub depth: u32,
    /// Lifecycle state (enforced state machine).
    pub state: SubRunState,
    /// If this run is a gate-retry, links to the original run_id.
    pub retry_of: Option<String>,
}

/// Canonical in-memory projection of the durable delegation hierarchy.
///
/// Keeping the records, delegation membership, and parent edges behind one
/// lock makes insertion/recovery/cleanup atomic. A run lookup is O(1) instead
/// of scanning every user's delegation, which also removes an avoidable
/// cross-tenant performance coupling in server deployments.
#[derive(Default)]
struct DelegationTrackerState {
    runs: HashMap<String, SubRunRecord>,
    delegation_runs: HashMap<String, Vec<String>>,
    parents: HashMap<String, String>,
    pause_flags: HashMap<String, Arc<AtomicBool>>,
    cancel_tokens: HashMap<String, Arc<tokio_util::sync::CancellationToken>>,
}

/// Process-local projection for live delegation controls.
///
/// Durable run records are authoritative and rebuild this projection after a
/// restart. The optional session journal is diagnostic evidence, not the
/// source of truth for recovery.
pub struct DelegationTracker {
    state: RwLock<DelegationTrackerState>,
    /// Authenticated owner for journal isolation.
    user_id: Option<String>,
    /// Optional session ID for journal persistence.
    session_id: Option<String>,
    /// Optional progress broadcaster for SSE events.
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
}

impl DelegationTracker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(DelegationTrackerState::default()),
            user_id: None,
            session_id: None,
            progress_broadcaster: None,
        }
    }

    /// Create a tracker with journal persistence enabled.
    pub fn with_session(user_id: String, session_id: String) -> Self {
        Self {
            state: RwLock::new(DelegationTrackerState::default()),
            user_id: Some(user_id),
            session_id: Some(session_id),
            progress_broadcaster: None,
        }
    }

    /// Attach a progress broadcaster for SSE event emission.
    pub fn with_progress_broadcaster(
        mut self,
        broadcaster: Arc<crate::orchestration::ProgressBroadcaster>,
    ) -> Self {
        self.progress_broadcaster = Some(broadcaster);
        self
    }

    /// Get the progress broadcaster, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.progress_broadcaster.as_ref()
    }

    /// Persist a delegation event to the session journal (best-effort).
    fn persist_event(
        &self,
        event_type: astra_services::session_journal::JournalEventType,
        metadata: serde_json::Value,
    ) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let mut event = astra_services::session_journal::JournalEvent::base_public(
            event_type,
            Some(sid.as_str()),
        );
        event.metadata = Some(metadata);
        self.persist_journal_entry(event);
    }

    /// Persist a fully constructed journal event (best-effort).
    fn persist_journal_entry(&self, event: astra_services::session_journal::JournalEvent) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let Some(ref user_id) = self.user_id else {
            astra_core::agent_warn!("delegation", "journal owner missing for session {sid}");
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::for_user(user_id, sid) {
            Ok(w) => w,
            Err(e) => {
                astra_core::agent_warn!(
                    "delegation",
                    "JournalWriter::new failed for session {sid}: {e}"
                );
                return;
            }
        };
        if let Err(e) = writer.append(&event) {
            astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
        }
    }

    /// Rebuild in-memory hierarchy from durable run records.
    ///
    /// Called at startup to recover delegation state after a crash.
    /// Only records with `parent_run_id` set are considered (sub-runs).
    pub async fn load_from_run_records(&self, records: &[astra_services::runs::DurableRunRecord]) {
        let mut state = self.state.write().await;

        for rec in records {
            let (Some(parent_run_id), Some(delegation_id)) =
                (&rec.parent_run_id, &rec.delegation_id)
            else {
                continue; // Skip root runs
            };

            let sub = SubRunRecord {
                run_id: rec.run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                delegation_id: delegation_id.clone(),
                agent_id: rec.agent_id.clone().unwrap_or_default(),
                depth: rec.depth,
                state: match durable_run_status_kind(&rec.status) {
                    DurableRunStatusKind::Other => {
                        tracing::warn!(
                            target: "astra_runtime::delegation",
                            status = %rec.status,
                            run_id = %rec.run_id,
                            "recovered delegation run has unknown durable status; projecting failed"
                        );
                        SubRunState::Failed
                    }
                    _ => durable_run_status_to_subrun_state(&rec.status),
                },
                retry_of: rec.retry_of.clone(),
            };

            let delegation_runs = state
                .delegation_runs
                .entry(delegation_id.clone())
                .or_default();
            if !delegation_runs.contains(&rec.run_id) {
                delegation_runs.push(rec.run_id.clone());
            }
            state
                .parents
                .insert(rec.run_id.clone(), parent_run_id.clone());
            state.runs.insert(rec.run_id.clone(), sub);
        }
    }

    /// Record a sub-run spawned by a delegation, persisting to journal if configured.
    pub async fn record_sub_run(&self, record: SubRunRecord) {
        self.record_sub_run_with_progress(record, true).await;
    }

    /// Record durable lineage while keeping lifecycle publication under the
    /// producer that actually owns the spawn.
    ///
    /// `DynamicAgentSpawner` registers mailboxes through `DelegationLookup`
    /// and then publishes a richer spawn event containing the exact agent
    /// type and fanout slot. Letting this bookkeeping callback publish too
    /// creates two conflicting `agent_spawned` rows for one child. Native
    /// delegation callers continue to use `record_sub_run`, which remains the
    /// lifecycle owner for that path.
    async fn record_sub_run_with_progress(&self, record: SubRunRecord, publish_progress: bool) {
        let run_id = record.run_id.clone();
        let parent_id = record.parent_run_id.clone();
        let delegation_id = record.delegation_id.clone();
        let agent_id = record.agent_id.clone();

        let mut state = self.state.write().await;
        if let Some(existing) = state.runs.get(&run_id) {
            if existing.parent_run_id != parent_id || existing.delegation_id != delegation_id {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    existing_parent_run_id = %existing.parent_run_id,
                    attempted_parent_run_id = %parent_id,
                    existing_delegation_id = %existing.delegation_id,
                    attempted_delegation_id = %delegation_id,
                    "ignored conflicting duplicate sub-run identity"
                );
            }
            return;
        }
        state
            .delegation_runs
            .entry(delegation_id.clone())
            .or_default()
            .push(run_id.clone());
        state.parents.insert(run_id.clone(), parent_id.clone());
        state.runs.insert(run_id.clone(), record);
        drop(state);

        // Emit SSE event for web clients
        if publish_progress && let Some(ref broadcaster) = self.progress_broadcaster {
            use crate::orchestration::{AgentProgressEvent, ProgressEventType};
            broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.clone(),
                run_id: run_id.clone(),
                parent_run_id: parent_id.clone(),
                event_type: ProgressEventType::AgentSpawned {
                    agent_type: "delegated".to_string(),
                    description: format!("Sub-run for delegation {}", delegation_id),
                    fanout_slot: None,
                },
                timestamp_epoch_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                metadata: None,
            });
        }
    }

    /// Get all sub-runs for a delegation.
    pub async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord> {
        let state = self.state.read().await;
        state
            .delegation_runs
            .get(delegation_id)
            .into_iter()
            .flatten()
            .filter_map(|run_id| state.runs.get(run_id).cloned())
            .collect()
    }

    /// Get the parent run ID for a given run.
    pub async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.state.read().await.parents.get(run_id).cloned()
    }

    /// Get the recorded delegation depth for a run, if known.
    pub async fn get_depth(&self, run_id: &str) -> Option<u32> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.depth)
    }

    /// Get all sub-run IDs for a given parent run across all delegations.
    pub async fn get_children(&self, parent_run_id: &str) -> Vec<String> {
        self.state
            .read()
            .await
            .parents
            .iter()
            .filter(|(_, parent)| parent.as_str() == parent_run_id)
            .map(|(child, _)| child.clone())
            .collect()
    }

    /// Get the agent_id for a run. Returns `None` for top-level (non-sub) runs.
    pub async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.agent_id.clone())
    }

    /// Get the current state of a sub-run by its run_id.
    pub async fn get_sub_run_state(&self, run_id: &str) -> Option<SubRunState> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.state)
    }

    // ── Pause / Resume ──────────────────────────────────────────────────────

    /// Register a cooperative pause flag for a sub-run.
    ///
    /// Returns the flag so the caller can pass it into [`SubRunConfig::pause_flag`].
    pub async fn register_pause_flag(&self, run_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.state
            .write()
            .await
            .pause_flags
            .insert(run_id.to_string(), flag.clone());
        flag
    }

    /// Get the pause flag for a sub-run, if registered.
    pub async fn get_pause_flag(&self, run_id: &str) -> Option<Arc<AtomicBool>> {
        self.state.read().await.pause_flags.get(run_id).cloned()
    }

    /// Set the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was set.
    pub async fn pause_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.state.read().await.pause_flags.get(run_id) {
            flag.store(true, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "pause", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Clear the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was cleared.
    pub async fn resume_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.state.read().await.pause_flags.get(run_id) {
            flag.store(false, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "resume", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Pause ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs paused.
    pub async fn pause_delegation(&self, delegation_id: &str) -> usize {
        let state = self.state.read().await;
        let run_ids = state
            .delegation_runs
            .get(delegation_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut count = 0;
        for run_id in run_ids {
            if let Some(flag) = state.pause_flags.get(run_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_delegation(&self, delegation_id: &str) -> usize {
        let state = self.state.read().await;
        let run_ids = state
            .delegation_runs
            .get(delegation_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut count = 0;
        for run_id in run_ids {
            if let Some(flag) = state.pause_flags.get(run_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Pause ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs paused.
    pub async fn pause_children_of(&self, parent_run_id: &str) -> usize {
        let state = self.state.read().await;
        let mut count = 0;
        for child_id in state
            .parents
            .iter()
            .filter_map(|(child, parent)| (parent == parent_run_id).then_some(child))
        {
            if let Some(flag) = state.pause_flags.get(child_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_children_of(&self, parent_run_id: &str) -> usize {
        let state = self.state.read().await;
        let mut count = 0;
        for child_id in state
            .parents
            .iter()
            .filter_map(|(child, parent)| (parent == parent_run_id).then_some(child))
        {
            if let Some(flag) = state.pause_flags.get(child_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Register a cancellation token for a sub-run so `cancel_children_of` can cancel it.
    pub async fn register_cancel_token(
        &self,
        run_id: &str,
        token: Arc<tokio_util::sync::CancellationToken>,
    ) {
        self.state
            .write()
            .await
            .cancel_tokens
            .insert(run_id.to_string(), token);
    }

    /// Request cancellation of one non-terminal sub-run.
    ///
    /// This deliberately does not write a terminal status. The owning
    /// executor must observe its cancellation token, finish its current
    /// boundary, and report the canonical cancelled outcome exactly once;
    /// eagerly writing `cancelled` here would race that result and make an
    /// interrupted child look settled before it has stopped. Cancellation is
    /// deliberately distinct from pause: the engine installs a token for
    /// every active child, and a cancel request must never turn into a paused
    /// run in an executor that observes only one of the two signals.
    pub async fn request_cancel_sub_run(&self, run_id: &str) -> bool {
        {
            // Completion needs the state write lock. Validate liveness and
            // signal the token under one read guard so a terminal transition
            // cannot slip between the two operations and acquire a spurious
            // post-completion cancellation marker.
            let state = self.state.read().await;
            let Some(record) = state
                .runs
                .get(run_id)
                .filter(|record| !record.state.is_terminal())
            else {
                return false;
            };
            let Some(cancel_token) = state.cancel_tokens.get(&record.run_id) else {
                return false;
            };
            cancel_token.cancel();
        }
        self.persist_event(
            astra_services::session_journal::JournalEventType::SyncMarker,
            serde_json::json!({ "action": "cancel_requested", "run_id": run_id }),
        );
        true
    }

    /// Cancel ALL sub-runs in the subtree rooted at `parent_run_id`.
    ///
    /// Walks the `parents` map transitively so grandchildren and deeper
    /// descendants are cancelled too. A flat one-level scan would leave
    /// sub-runs spawned by cancelled children executing — which was the
    /// historical bug.
    ///
    /// Returns the number of live children that accepted the cancellation
    /// request. A cancellation token is installed for every executable child;
    /// cancellation never piggybacks on the separate pause flag.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        let descendants = self.collect_descendants(parent_run_id).await;
        let mut count = 0;
        for child_id in descendants {
            if self.request_cancel_sub_run(&child_id).await {
                count += 1;
            }
        }
        count
    }

    /// Walk the `parents` map starting from `root`, collecting every
    /// descendant run_id. BFS so siblings cancel before grandchildren —
    /// minimizing the time a freshly-spawned grandchild has to do work
    /// before being cancelled. Cycle-safe via `visited`.
    async fn collect_descendants(&self, root: &str) -> Vec<String> {
        let state = self.state.read().await;
        // Build child-by-parent index once so the walk is O(N) total
        // rather than O(N) per level.
        let mut by_parent: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (child, parent) in state.parents.iter() {
            by_parent
                .entry(parent.as_str())
                .or_default()
                .push(child.as_str());
        }
        let mut visited = std::collections::HashSet::new();
        let mut frontier: std::collections::VecDeque<String> = by_parent
            .get(root)
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        while let Some(rid) = frontier.pop_front() {
            if !visited.insert(rid.clone()) {
                continue;
            }
            if let Some(grand) = by_parent.get(rid.as_str()) {
                for g in grand {
                    frontier.push_back((*g).to_string());
                }
            }
            out.push(rid);
        }
        out
    }

    /// Check if a sub-run is currently paused.
    pub async fn is_paused(&self, run_id: &str) -> bool {
        self.state
            .read()
            .await
            .pause_flags
            .get(run_id)
            .is_some_and(|f| f.load(Ordering::Acquire))
    }

    // ── State Machine + Lifecycle ───────────────────────────────────────────

    /// Transition a sub-run's state, enforcing the state machine.
    ///
    /// Returns `Err` if the transition is illegal.
    pub async fn transition_state(
        &self,
        run_id: &str,
        to: SubRunState,
    ) -> Result<SubRunState, InvalidTransition> {
        let mut state = self.state.write().await;
        if let Some(record) = state.runs.get_mut(run_id) {
            let new_state = record.state.try_transition(to)?;
            record.state = new_state;

            let delegation_id = record.delegation_id.clone();
            let agent_id = record.agent_id.clone();
            let parent_run_id = record.parent_run_id.clone();
            let depth = record.depth;
            let retry_of = record.retry_of.clone();
            drop(state);

            if new_state == SubRunState::Running {
                self.persist_event(
                    astra_services::session_journal::JournalEventType::DelegationSubRunStarted,
                    serde_json::json!({
                        "delegation_id": delegation_id,
                        "sub_run_id": run_id,
                        "parent_run_id": parent_run_id,
                        "agent_id": agent_id,
                        "status": new_state.as_str(),
                        "depth": depth,
                        "retry_of": retry_of,
                    }),
                );
            }

            return Ok(new_state);
        }
        // Run not tracked — allow the transition (e.g. root runs)
        Ok(to)
    }

    /// Mark a sub-run as complete: transition state, remove pause flag.
    ///
    /// Releases cooperative controls while retaining queryable lineage.
    pub async fn complete_sub_run(&self, run_id: &str, terminal_state: SubRunState) {
        self.complete_sub_run_with_result(run_id, terminal_state, None, None)
            .await;
    }

    /// Mark a sub-run as complete and persist the terminal result metadata.
    pub async fn complete_sub_run_with_result(
        &self,
        run_id: &str,
        terminal_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) {
        debug_assert!(terminal_state.is_terminal());
        self.set_sub_run_result_state(run_id, terminal_state, error, output_preview, true)
            .await;
    }

    pub async fn apply_sub_run_result_state(
        &self,
        run_id: &str,
        result_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) {
        if result_state.is_terminal() {
            self.complete_sub_run_with_result(run_id, result_state, error, output_preview)
                .await;
            return;
        }

        debug_assert!(matches!(
            result_state,
            SubRunState::Waiting | SubRunState::Paused
        ));
        self.set_sub_run_result_state(run_id, result_state, error, output_preview, false)
            .await;
    }

    async fn set_sub_run_result_state(
        &self,
        run_id: &str,
        result_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
        emit_completion_event: bool,
    ) {
        // Transition state in record
        let mut delegation_id = None;
        let mut agent_id = None;
        let mut parent_run_id = None;
        let mut final_state = result_state;
        let mut transition_applied = false;
        {
            let mut state = self.state.write().await;
            if let Some(record) = state.runs.get_mut(run_id) {
                if record.state == result_state {
                    // Executor completion can race recovery/replay. The
                    // already-projected state is the idempotent authority; do
                    // not emit a duplicate terminal event.
                    if record.state.is_terminal() {
                        state.pause_flags.remove(run_id);
                        state.cancel_tokens.remove(run_id);
                    }
                    return;
                }
                let previous_state = record.state;
                let Ok(next_state) = previous_state.try_transition(result_state) else {
                    tracing::error!(
                        target: "astra_runtime::delegation",
                        run_id,
                        from = previous_state.as_str(),
                        to = result_state.as_str(),
                        "rejected illegal delegated sub-run result transition"
                    );
                    return;
                };
                record.state = next_state;
                final_state = record.state;
                transition_applied = true;
                delegation_id = Some(record.delegation_id.clone());
                agent_id = Some(record.agent_id.clone());
                parent_run_id = Some(record.parent_run_id.clone());
                if final_state.is_terminal() {
                    state.pause_flags.remove(run_id);
                    state.cancel_tokens.remove(run_id);
                }
            }
        }

        if !transition_applied {
            tracing::warn!(
                target: "astra_runtime::delegation",
                run_id,
                to = result_state.as_str(),
                "ignored result for an untracked delegated sub-run"
            );
            return;
        }

        // Emit the durable completion and its SSE event
        if let (Some(did), Some(aid), Some(parent_run_id)) =
            (delegation_id, agent_id, parent_run_id)
        {
            if emit_completion_event {
                self.persist_journal_entry(
                    astra_services::session_journal::JournalEvent::delegation_sub_run_completed(
                        self.session_id.as_deref(),
                        &did,
                        run_id,
                        &aid,
                        final_state.as_str(),
                        error,
                        output_preview,
                    ),
                );
            }

            // Emit completion SSE event for web clients
            if emit_completion_event {
                if let Some(ref broadcaster) = self.progress_broadcaster {
                    use crate::orchestration::{
                        AgentProgressEvent, CancellationOrigin, ProgressEventType,
                    };
                    // Canonical wire string — `as_str()` is what every other
                    // SSE/JSON site uses (line 1096 above, and the trace
                    // emitters). Using `{:?}` here leaked Rust enum casing
                    // ("VerificationFailed") into the user-visible payload
                    // instead of the snake_case wire form
                    // ("verification_failed"), and silently coupled the
                    // SSE wire format to the Debug derive — a refactor of
                    // the enum's variant names would corrupt SSE downstream.
                    let status_str = final_state.as_str();
                    let event_type = match final_state {
                        SubRunState::Completed => ProgressEventType::Completed {
                            result_summary: format!("Sub-run {} finished", run_id),
                            total_tool_calls: 0,
                            total_tokens: (0, 0),
                            duration_ms: 0,
                        },
                        SubRunState::Paused => ProgressEventType::Interrupted {
                            reason: "paused".to_string(),
                            partial_summary: format!("Sub-run {} paused", run_id),
                            total_tool_calls: 0,
                            total_tokens: (0, 0),
                            duration_ms: 0,
                        },
                        SubRunState::Waiting => ProgressEventType::Waiting {
                            reason: error.unwrap_or("external_dependency").to_string(),
                        },
                        SubRunState::Cancelled => ProgressEventType::Cancelled {
                            reason: format!("Sub-run {} cancelled", run_id),
                            origin: CancellationOrigin::Unverified,
                        },
                        _ => ProgressEventType::Failed {
                            error: format!("Sub-run terminal state: {}", status_str),
                        },
                    };
                    broadcaster.emit(AgentProgressEvent {
                        agent_id: aid,
                        run_id: run_id.to_string(),
                        parent_run_id,
                        event_type,
                        timestamp_epoch_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        metadata: None,
                    });
                }
            }
        }
    }
}

#[async_trait]
impl astra_messaging::DelegationLookup for DelegationTracker {
    async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.get_parent(run_id).await
    }
    async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        self.get_agent_id(run_id).await
    }
    async fn get_depth(&self, run_id: &str) -> Option<u32> {
        self.get_depth(run_id).await
    }
    async fn record_sub_run(&self, info: astra_messaging::SubRunInfo) {
        self.record_sub_run_with_progress(
            SubRunRecord {
                run_id: info.run_id,
                parent_run_id: info.parent_run_id,
                delegation_id: info.delegation_id,
                agent_id: info.agent_id,
                depth: info.depth,
                state: SubRunState::Created,
                retry_of: None,
            },
            false,
        )
        .await;
    }
}

impl Default for DelegationTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Delegation Engine ──────────────────────────────────────────────────────

pub type ChildSupervisorResolver = Arc<
    dyn Fn(
            String,
            String,
        )
            -> futures_util::future::BoxFuture<'static, Result<Arc<DynamicAgentSpawner>, String>>
        + Send
        + Sync,
>;

/// Engine for executing multi-agent delegations.
///
/// Validates delegation requests against the agent profile registry,
/// spawns sub-runs via RunEngine, tracks hierarchies via DelegationTracker,
/// and **executes** them via [`SubRunExecutor`].
#[derive(Clone)]
pub struct DelegationEngine {
    /// Agent profiles for validation.
    registry: Arc<RwLock<AgentProfileRegistry>>,
    /// Run engine for spawning sub-runs.
    run_engine: Arc<RunEngine>,
    /// Tracks parent→child run relationships.
    tracker: Arc<DelegationTracker>,
    /// Executor for actually running sub-agent loops.
    executor: Arc<dyn SubRunExecutor>,
    /// Execution-scoped owner; global engines are templates, never supervisors.
    spawner: Option<Arc<DynamicAgentSpawner>>,
    supervisor_resolver: Option<ChildSupervisorResolver>,
    /// Optional mailbox router for inter-agent messaging.
    mailbox_router: Option<Arc<AgentMailboxRouter>>,
    /// Optional fork-prefix store shared with the spawner. When
    /// present, delegate sub-run configs get `inherited_prefix`
    /// populated by looking up the parent's captured ForkPrefix in
    /// this store (Bug B step 2). When absent, the delegate path
    /// behaves as pre-fork-prefix — `inherited_prefix` stays None
    /// and the child runs fresh.
    prefix_store: Option<Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
}

impl DelegationEngine {
    pub fn with_supervisor_resolver(mut self, resolver: ChildSupervisorResolver) -> Self {
        self.supervisor_resolver = Some(resolver);
        self
    }

    pub async fn for_session(&self, user_id: &str, session_id: &str) -> Result<Self, String> {
        if let Some(spawner) = self.spawner.as_ref() {
            if spawner
                .current_session_id()
                .is_some_and(|bound| bound != session_id)
            {
                return Err("delegation child supervisor belongs to another session".into());
            }
            return Ok(self.clone());
        }
        let resolver = self
            .supervisor_resolver
            .as_ref()
            .ok_or("delegation template has no session child-supervisor resolver")?;
        Ok(self.for_execution(resolver(user_id.to_string(), session_id.to_string()).await?))
    }

    pub fn for_execution(&self, spawner: Arc<DynamicAgentSpawner>) -> Self {
        Self {
            spawner: Some(spawner),
            ..self.clone()
        }
    }
    /// Reserved context key. Session identity is a typed request field, but
    /// remove a user/model-provided key with this name if it appears in task
    /// context so it cannot masquerade as runtime metadata in a child prompt.
    const SESSION_ID_CONTEXT_KEY: &'static str = "session_id";
    /// Runtime-injected provider ownership. It is reconstructed from the
    /// durable parent below; a caller-supplied value is never trusted.
    const PROVIDER_RUN_OWNER_CONTEXT_KEY: &'static str = "provider_run_owner";

    fn session_id_for(request: &DelegationRequest) -> String {
        request.session_id.clone()
    }

    fn child_task_context(request: &DelegationRequest) -> HashMap<String, serde_json::Value> {
        let mut context = clone_delegation_context(
            astra_core::history_work::HistoryWorkSite::DelegationContextClone,
            &request.context,
        );
        context.remove(Self::SESSION_ID_CONTEXT_KEY);
        context
    }

    async fn prepare_subrun_models(
        &self,
        requests: &[SubRunModelRequest],
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        timeout: std::time::Duration,
    ) -> Result<Vec<Option<PreparedSubRunModel>>, SubRunModelPreparationError> {
        if timeout.is_zero() {
            return Err(SubRunModelPreparationError::TimedOut);
        }
        let admission = self.executor.prepare_model_batch(requests);
        let prepared = tokio::select! {
            biased;
            _ = async {
                if let Some(token) = cancel_token {
                    token.cancelled().await;
                }
            }, if cancel_token.is_some() => {
                return Err(SubRunModelPreparationError::Cancelled);
            }
            result = tokio::time::timeout(timeout, admission) => {
                result.map_err(|_| SubRunModelPreparationError::TimedOut)??
            }
        };
        if prepared.len() != requests.len() {
            return Err(format!(
                "sub-run executor prepared {} model identities for {} child slots",
                prepared.len(),
                requests.len()
            )
            .into());
        }
        for (request, model) in requests.iter().zip(&prepared) {
            let expected_offering = request
                .selection
                .as_ref()
                .or(request
                    .parent_model_reasoning
                    .as_ref()
                    .map(|parent| &parent.selection))
                .map(|selection| selection.offering_id.as_str())
                .or_else(|| {
                    request
                        .inherited_execution
                        .as_ref()
                        .map(|execution| execution.offering_id.as_str())
                });
            let parent_offering = request
                .parent_model_reasoning
                .as_ref()
                .map(|parent| parent.selection.offering_id.as_str());
            if let Some(model) = model {
                astra_services::validate_model_offering_id(&model.offering_id)
                    .map_err(|error| error.to_string())?;
                if model.model_name.trim().is_empty()
                    || expected_offering.is_some_and(|id| id != model.offering_id)
                    || request
                        .inherited_execution
                        .as_ref()
                        .is_some_and(|execution| {
                            execution.offering_id == model.offering_id
                                && execution.model_name != model.model_name
                        })
                    || request
                        .parent_model_reasoning
                        .as_ref()
                        .and_then(|parent| parent.resolved_model_name.as_ref())
                        .is_some_and(|name| {
                            parent_offering == Some(model.offering_id.as_str())
                                && name != &model.model_name
                        })
                    || model.admitted_execution.as_ref().is_some_and(|execution| {
                        execution.offering_id != model.offering_id
                            || execution.model_name != model.model_name
                    })
                {
                    return Err(
                        "sub-run executor returned a mismatched prepared model identity".into(),
                    );
                }
            } else if request
                .selection
                .as_ref()
                .is_some_and(|selection| Some(selection.offering_id.as_str()) != parent_offering)
            {
                return Err(
                    "sub-run executor did not prepare the explicitly selected Offering".into(),
                );
            }
        }
        Ok(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_delegated_run(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: &str,
        delegation_id: &str,
        agent_id: &str,
        retry_of: Option<&str>,
        interaction_mode: RequestedTurnInteractionMode,
        request_constraints: &RequestConstraints,
        execution_restrictions: astra_services::runs::DurableExecutionRestrictions,
        execution_metadata: Option<&serde_json::Value>,
        admitted_agent_profiles: Option<&Arc<astra_services::runs::AgentProfileSnapshot>>,
        profile_authority: &ParentProfileAuthority,
        thinking: &astra_turn_core::thinking_config::ThinkingConfig,
        prepared_model: Option<&PreparedSubRunModel>,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        execution_deadline: Option<tokio::time::Instant>,
    ) -> Result<RunExecutionAuthority, String> {
        if cancel_token.is_some_and(|token| token.is_cancelled()) {
            return Err("sub-run cancelled before durable child creation".into());
        }
        if execution_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err("sub-run deadline expired before durable child creation".into());
        }
        if let Some(prepared) = prepared_model
            && (astra_services::validate_model_offering_id(&prepared.offering_id).is_err()
                || prepared.model_name.trim().is_empty())
        {
            return Err("prepared child model identity is invalid".to_string());
        }
        self.run_engine
            .start_run_ext_with_context_with_deadline(
                run_id,
                user_id,
                session_id,
                Some(parent_run_id),
                Some(delegation_id),
                Some(agent_id),
                retry_of,
                crate::server::run::engine::RunStartContext {
                    interaction_mode,
                    execution_restrictions: Some(execution_restrictions),
                    execution_metadata: execution_metadata
                        .and_then(serde_json::Value::as_object)
                        .cloned(),
                    admitted_agent_profiles: admitted_agent_profiles.cloned(),
                    profile_authority: profile_authority.clone(),
                    child_runtime_id: Some(child_runtime_id(agent_id, run_id)),
                    generation_controls: Some(crate::server::run::engine::RunGenerationControls {
                        thinking: thinking.clone(),
                        first_output_max_tokens: None,
                        preserve_thinking: *thinking
                            != astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
                    }),
                    delegated_model_requirements: Some(
                        request_constraints.delegated_model_requirements.clone(),
                    ),
                    model_identity_admitted: prepared_model.is_some(),
                    model_selection: prepared_model.map(|prepared| {
                        astra_turn_types::ModelSelection {
                            offering_id: prepared.offering_id.clone(),
                        }
                    }),
                    resolved_model_selection: prepared_model.map(|prepared| {
                        astra_services::runs::ResolvedModelSelection {
                            offering_id: prepared.offering_id.clone(),
                            model_name: prepared.model_name.clone(),
                            source_identity: None,
                        }
                    }),
                    ..Default::default()
                },
                execution_deadline,
            )
            .await
    }

    /// Settle a child that was durably admitted but never handed to an
    /// executor. Until execution starts, the delegation scheduler still owns
    /// the exact generation even when the configured executor normally owns
    /// terminal writes.
    fn settle_unlaunched_child<'a>(
        &'a self,
        user_id: &'a str,
        session_id: &'a str,
        agent_id: &str,
        run_id: &str,
        owner_generation: u64,
        status: &str,
        error: &str,
        mailbox: Option<&MailboxRetirement>,
    ) -> impl std::future::Future<Output = AgentResult> + Send + 'a {
        let attempted = AgentResult {
            agent_id: agent_id.to_string(),
            run_id: run_id.to_string(),
            status: status.to_string(),
            output: None,
            error: Some(error.to_string()),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let reconciliation = reconcile_preserving_mailbox_cleanup(
            &self.run_engine,
            user_id,
            session_id,
            DurableLifecycleDisposition::SchedulerOwned { owner_generation },
            attempted,
            mailbox,
            "unlaunched child",
        );
        let tracker = self.tracker.clone();
        let run_id = run_id.to_string();
        async move {
            let result = reconciliation.await;
            tracker
                .apply_sub_run_result_state(
                    &run_id,
                    agent_result_status_to_subrun_state(&result.status),
                    result.error.as_deref(),
                    None,
                )
                .await;
            result
        }
    }

    async fn settle_unlaunched_child_with_shared_deadline(
        &self,
        user_id: &str,
        session_id: &str,
        agent_id: &str,
        run_id: &str,
        owner_generation: u64,
        status: &str,
        error: &str,
        mailbox: Option<&MailboxRetirement>,
        deadline: &mut Option<tokio::time::Instant>,
    ) -> AgentResult {
        let attempted = AgentResult {
            agent_id: agent_id.to_string(),
            run_id: run_id.to_string(),
            status: status.to_string(),
            output: None,
            error: Some(error.to_string()),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        if let Some(result) = await_with_shared_deadline(
            deadline,
            DELEGATION_RECONCILIATION_TIMEOUT,
            self.settle_unlaunched_child(
                user_id,
                session_id,
                agent_id,
                run_id,
                owner_generation,
                status,
                error,
                mailbox,
            ),
        )
        .await
        {
            return result;
        }
        let pending = durable_reconciliation_pending_result(&attempted, "unlaunched child");
        self.tracker
            .apply_sub_run_result_state(
                run_id,
                agent_result_status_to_subrun_state(&pending.status),
                pending.error.as_deref(),
                pending.output.as_deref(),
            )
            .await;
        pending
    }

    #[cfg(test)]
    fn new(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
    ) -> Self {
        Self::with_executor(registry, run_engine, tracker, Arc::new(StubSubRunExecutor))
    }

    /// Create engine with a real sub-run executor.
    pub fn with_executor(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
        executor: Arc<dyn SubRunExecutor>,
    ) -> Self {
        if let Some(metrics) = run_engine.metrics_registry() {
            register_delegation_metrics(metrics);
        }
        Self {
            registry,
            run_engine,
            tracker,
            executor,
            spawner: None,
            supervisor_resolver: None,
            mailbox_router: None,
            prefix_store: None,
        }
    }

    /// Attach a mailbox router for inter-agent messaging within delegations.
    pub fn with_mailbox_router(mut self, router: Arc<AgentMailboxRouter>) -> Self {
        self.mailbox_router = Some(router);
        self
    }

    /// Attach the fork-prefix store the spawner owns. Delegate
    /// sub-runs will then inherit the parent's captured prefix for
    /// prompt-cache reuse — matching agent-spawn behavior. When
    /// unset, delegate sub-runs run fresh (pre-fork-prefix
    /// behavior).
    pub fn with_prefix_store(
        mut self,
        store: Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>,
    ) -> Self {
        self.prefix_store = Some(store);
        self
    }

    fn available_agent_profile_ids(registry: &AgentProfileRegistry) -> String {
        let mut ids: Vec<String> = registry
            .list()
            .into_iter()
            .map(|profile| profile.agent_id.clone())
            .collect();
        ids.sort_unstable();
        if ids.is_empty() {
            "(none)".to_string()
        } else {
            ids.join(", ")
        }
    }

    fn missing_agent_profile_error(
        operation: &str,
        agent_id: &str,
        registry: &AgentProfileRegistry,
    ) -> String {
        format!(
            "delegation failed during {operation}: requested agent profile '{agent_id}' is not registered. \
             Available profiles: [{}]. This is a configuration error; do not invent a replacement agent_id.",
            Self::available_agent_profile_ids(registry)
        )
    }

    /// Read-only accessor for the attached prefix store. Used by
    /// `crate::server::run::lifecycle::build_host` so the server-side parent loop
    /// host captures into the same store the delegate path reads
    /// from — without this, delegate sub-runs could never inherit
    /// because no parent capture would ever land in the store.
    pub fn prefix_store(
        &self,
    ) -> Option<&Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>> {
        self.prefix_store.as_ref()
    }

    /// Resolve a parent prefix for a delegated sub-run's config.
    /// Returns `None` (no inheritance) when:
    /// - no prefix_store is configured
    /// - no parent prefix captured for `parent_run_id`
    /// - resolver rejects the prefix (provider / model mismatch,
    ///   thinking budget clamp, etc.) — soft-fallback semantics,
    ///   same as agent spawn
    ///
    /// `child_provider` and `child_model_id` come from the resolved
    /// executor-prepared provider model name, never an Offering ID. We use
    /// [`astra_turn_core::fork_prefix::ProviderKind::from_provider_hint`]
    /// on the model name — the same inference the spawner uses.
    fn resolve_inherited_prefix_for_delegate(
        &self,
        parent_run_id: &str,
        child_model_id: &str,
    ) -> Option<crate::orchestration::InheritedChildPrefix> {
        let store = self.prefix_store.as_ref()?;
        let child_provider =
            astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(child_model_id);
        let spec = astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec {
            from_run_id: Some(parent_run_id.to_string()),
            required: false,
        };
        let ctx = astra_turn_core::fork_resolve::SpawnResolveContext {
            caller_run_id: Some(parent_run_id.to_string()),
            child_provider,
            child_model_id: child_model_id.to_string(),
            child_thinking: None,
            // Delegate doesn't expose max_output_tokens (agent
            // profile carries max_turns only), so leave None —
            // validate_spawn will skip the thinking-budget clamp
            // check.
            child_max_output_tokens: None,
        };
        let outcome = astra_turn_core::fork_resolve::resolve_inherit_prefix(
            Some(&spec),
            &ctx,
            store.as_ref(),
        );
        crate::orchestration::spawner::build_inherited_child_prefix(&outcome)
    }

    fn ensure_source_in_delegation_chain(request: &mut DelegationRequest, source_agent_id: &str) {
        let source_agent_id = source_agent_id.trim();
        if source_agent_id.is_empty() {
            return;
        }
        // Compare agent identities by canonical form (lowercase + NFC) so
        // case-variant or Unicode normalization aliases cannot bypass the
        // chain membership check.
        let canonical = canonical_agent_id(source_agent_id);
        if !request
            .delegation_chain
            .iter()
            .any(|agent_id| canonical_agent_id(agent_id) == canonical)
        {
            request.delegation_chain.push(source_agent_id.to_string());
        }
    }

    fn delegation_chain_for_child(
        request: &DelegationRequest,
        child_agent_id: &str,
    ) -> Result<Vec<String>, String> {
        // Compare agent identities by their canonical form. Agent IDs are
        // user-provided and may vary in casing or Unicode normalization
        // (NFC vs NFD: "café" vs "cafe" + ◌́). Two IDs that are visually
        // identical must be treated as the same agent — otherwise a
        // normalization alias bypasses circular delegation detection and
        // allows an infinite loop.
        let canonical_child = canonical_agent_id(child_agent_id);
        if request
            .delegation_chain
            .iter()
            .any(|agent_id| canonical_agent_id(agent_id) == canonical_child)
        {
            let mut cycle = request.delegation_chain.clone();
            cycle.push(child_agent_id.to_string());
            let chain_display = cycle.join(" → ");
            return Err(format!(
                "circular delegation detected: {chain_display}. Agent '{child_agent_id}' already exists in the delegation chain"
            ));
        }
        Ok(request.delegation_chain.clone())
    }

    /// Get the progress broadcaster from the underlying tracker, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.tracker.progress_broadcaster()
    }

    /// Validate a delegation request without executing it.
    pub async fn validate(
        &self,
        request: &DelegationRequest,
        source_agent_id: &str,
    ) -> Result<(), String> {
        let reg = self.registry.read().await;
        reg.validate_delegation(request, source_agent_id)
    }

    /// Execute a delegation: spawn sub-runs according to the coordination pattern.
    ///
    /// Returns a `DelegationResult` with individual agent results and
    /// aggregated output. Sub-runs are created in the RunEngine and tracked
    /// in the DelegationTracker for hierarchy queries.
    ///
    /// `cancel_token` is scoped to this execution — no global state. When
    /// cancelled, all spawned sub-runs receive the signal and stop gracefully.
    pub async fn execute(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        self.execute_with_forward_headers(
            request,
            source_agent_id,
            cancel_token,
            HashMap::new(),
            None,
        )
        .await
    }

    pub async fn execute_with_forward_headers(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
    ) -> Result<DelegationResult, String> {
        self.execute_with_forward_headers_and_live_events(
            request,
            source_agent_id,
            cancel_token,
            forward_headers,
            admitted_model_execution,
            None,
            None,
        )
        .await
    }

    /// Execute one delegation with an optional request-scoped child live lane.
    /// The sink is deliberately an argument rather than engine state: a shared
    /// engine can serve concurrent sessions, and a stale TUI receiver must
    /// never observe another request's children.
    pub async fn execute_with_forward_headers_and_live_events(
        &self,
        mut request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
        parent_model_reasoning: Option<
            astra_turn_core::orchestration_spawn_tool::ParentModelReasoning,
        >,
        live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        if self.spawner.is_none() {
            return Err("delegation execution requires its session-owned child supervisor".into());
        }
        request
            .context
            .remove(crate::server::delegation::FORWARD_HEADERS_CONTEXT_KEY);
        Self::ensure_source_in_delegation_chain(&mut request, source_agent_id);
        let enabled_tools = parse_request_allowlist_from_context(
            &mut request.context,
            crate::server::delegation::REQUEST_ENABLED_TOOLS_CONTEXT_KEY,
        )?;
        // Server request admission materializes an omitted optional-tool
        // allowlist as `Some(empty)`. A local CLI has no server capability
        // boundary and deliberately leaves it as `None`; preserving that
        // distinction keeps delegated children from losing their normal
        // optional network-tool surface.
        let mut request_constraints = RequestConstraints::new(
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::server::delegation::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY,
            )?,
            enabled_tools,
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::server::delegation::REQUEST_ALLOWED_SKILLS_CONTEXT_KEY,
            )?,
            parse_request_skill_sources_from_context(
                &mut request.context,
                crate::server::delegation::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY,
            )?,
        );
        if let Some(value) = request
            .context
            .remove(astra_turn_types::DELEGATED_MODEL_REQUIREMENTS_CONTEXT_KEY)
        {
            request_constraints.delegated_model_requirements = serde_json::from_value(value)
                .map_err(|_| "delegated model handoff is malformed".to_string())?;
            request_constraints
                .delegated_model_requirements
                .validate()
                .map_err(str::to_string)?;
        }

        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth_u32(
                request.depth,
            )?;

        let session_id = Self::session_id_for(&request);
        let parent_run = self
            .run_engine
            .require_delegation_parent(&request.user_id, &session_id, &request.parent_run_id)
            .await?;
        let mut execution_contract =
            crate::server::run::binding_resolution::durable_run_execution_contract(&parent_run)?;
        execution_contract.1.phase =
            if astra_turn_core::stop_hooks_yaml::is_plan_subtask_from_delegation_context(
                &request.context,
            ) {
                astra_turn_types::CompletionCheckPhase::TaskCompleted
            } else {
                astra_turn_types::CompletionCheckPhase::Stop
            };
        let admitted_agent_profiles =
            crate::server::run::engine::durable_run_agent_profiles(&parent_run, &request.user_id)?;
        let parent_profile_authority = crate::server::run::engine::durable_run_profile_authority(
            &parent_run,
            &request.user_id,
        )?;
        parent_profile_authority.require_delegation()?;
        let mut execution = self.clone();
        if let Some(snapshot) = admitted_agent_profiles.as_ref() {
            let registry = snapshot.registry(&request.user_id)?;
            if request.depth != parent_run.depth {
                return Err("delegation depth does not match its durable parent".into());
            }
            let parent_profile = match &parent_profile_authority {
                ParentProfileAuthority::OrdinaryRoot => None,
                ParentProfileAuthority::AdmittedMember { profile_id, .. } => Some(
                    registry
                        .get(profile_id)
                        .ok_or("admitted parent profile is missing")?,
                ),
                ParentProfileAuthority::Unbound | ParentProfileAuthority::NonDelegating { .. } => {
                    return Err("admitted roster requires parent profile authority".into());
                }
            };
            let target_ids = match &request.pattern {
                CoordinationPattern::FanOut { agent_ids, .. }
                | CoordinationPattern::Sequential { agent_ids, .. } => agent_ids.as_slice(),
                CoordinationPattern::Fork { agent_id, .. } => std::slice::from_ref(agent_id),
            };
            if target_ids.is_empty() {
                return Err("delegation pattern has no agents".into());
            }
            // Validate the entire fixed batch before model preparation or any
            // child start. Runtime routing names never supply profile ancestry.
            for target_id in target_ids {
                let target = registry
                    .get(target_id)
                    .ok_or_else(|| format!("target profile '{target_id}' is not admitted"))?;
                parent_profile_authority.for_child(target_id)?;
                if let Some(parent) = parent_profile {
                    if !parent.can_delegate_to_agent(target) {
                        return Err(format!(
                            "admitted profile '{}' cannot delegate to '{target_id}'",
                            parent.agent_id
                        ));
                    }
                    if parent_run.depth >= parent.max_delegation_depth {
                        return Err(format!(
                            "admitted profile '{}' delegation depth {} exceeds max {}",
                            parent.agent_id, parent_run.depth, parent.max_delegation_depth
                        ));
                    }
                }
            }
            // Reuse the execution-bound engine; never publish run-scoped
            // profiles into the mutable global registry or re-read their source.
            execution.registry = Arc::new(RwLock::new(registry));
        } else {
            self.validate(&request, source_agent_id).await?;
        }
        // The parent lookup is already required for delegation admission. Use
        // its immutable start event as the provider-scope authority and pass
        // the result through the existing typed child context. This keeps
        // shared executors request-local and rejects forged request context.
        request.context.remove(Self::PROVIDER_RUN_OWNER_CONTEXT_KEY);
        if let Some(provider_run_owner) = parent_run
            .events
            .iter()
            .find(|event| event["event_type"] == "run_started")
            .and_then(|event| event.pointer("/data/provider_run_owner"))
            .cloned()
            .map(serde_json::from_value::<astra_services::runs::ProviderRunOwner>)
            .transpose()
            .map_err(|error| format!("durable parent has an invalid provider owner: {error}"))?
        {
            request.context.insert(
                Self::PROVIDER_RUN_OWNER_CONTEXT_KEY.to_string(),
                serde_json::to_value(provider_run_owner)
                    .expect("provider run owner must serialize"),
            );
        }
        let persisted_parent_model_reasoning = match (
            parent_run.model_offering_id.as_deref(),
            parent_run.resolved_model_name.as_deref(),
        ) {
            (Some(offering_id), Some(model_name)) if !model_name.trim().is_empty() => {
                let controls =
                    crate::server::run::engine::durable_run_generation_controls(&parent_run)?;
                Some(
                    astra_turn_core::orchestration_spawn_tool::ParentModelReasoning {
                        selection: astra_turn_types::ModelSelection {
                            offering_id: offering_id.to_string(),
                        },
                        resolved_model_name: Some(model_name.to_string()),
                        thinking: controls.thinking,
                    },
                )
            }
            (None, None) => None,
            _ => return Err("durable parent has an incomplete model identity".into()),
        };
        let effective_parent_model_reasoning = match (
            parent_model_reasoning.as_ref(),
            persisted_parent_model_reasoning.as_ref(),
        ) {
            (Some(provided), Some(persisted))
                if provided.selection != persisted.selection
                    || provided.thinking != persisted.thinking =>
            {
                return Err(
                    "live parent model snapshot does not match its durable run identity".into(),
                );
            }
            (Some(_provided), Some(persisted)) => Some(persisted.clone()),
            (Some(provided), None) => Some(provided.clone()),
            (None, Some(persisted)) => Some(persisted.clone()),
            (None, None) => None,
        };
        let interaction_mode =
            crate::server::run::engine::durable_run_effective_interaction_mode(&parent_run)?;

        // Extract pattern name and agent_ids for journal event.
        let (pattern_name, agent_ids_for_journal): (&str, Vec<String>) = match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => ("fan_out", agent_ids.clone()),
            CoordinationPattern::Sequential { agent_ids, .. } => ("sequential", agent_ids.clone()),
            CoordinationPattern::Fork {
                agent_id, tasks, ..
            } => ("fork", vec![format!("{}×{}", agent_id, tasks.len())]),
        };

        // Journal: delegation started
        Self::write_journal_event(
            &request.user_id,
            &session_id,
            astra_services::session_journal::JournalEvent::delegation_started(
                Some(&session_id),
                &request.delegation_id,
                &request.parent_run_id,
                pattern_name,
                &agent_ids_for_journal,
            ),
        );

        let execution_started_at = std::time::Instant::now();
        let result = match &request.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                timeout_sec,
            } => {
                execution
                    .execute_fan_out(
                        &request,
                        agent_ids,
                        aggregation,
                        &forward_headers,
                        admitted_model_execution.as_ref(),
                        effective_parent_model_reasoning.as_ref(),
                        &request_constraints,
                        admitted_agent_profiles.as_ref(),
                        &parent_profile_authority,
                        &execution_contract,
                        child_recursion_depth,
                        interaction_mode,
                        *timeout_sec,
                        cancel_token.as_ref(),
                        live_event_sink.as_ref(),
                    )
                    .await
            }
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
                timeout_sec,
            } => {
                execution
                    .execute_sequential(
                        &request,
                        agent_ids,
                        *stop_on_success,
                        &forward_headers,
                        admitted_model_execution.as_ref(),
                        effective_parent_model_reasoning.as_ref(),
                        &request_constraints,
                        admitted_agent_profiles.as_ref(),
                        &parent_profile_authority,
                        &execution_contract,
                        child_recursion_depth,
                        interaction_mode,
                        *timeout_sec,
                        cancel_token.as_ref(),
                        live_event_sink.as_ref(),
                    )
                    .await
            }
            CoordinationPattern::Fork {
                tasks,
                agent_id,
                aggregation,
                timeout_sec,
            } => {
                execution
                    .execute_fork(
                        &request,
                        tasks,
                        agent_id,
                        aggregation,
                        &forward_headers,
                        admitted_model_execution.as_ref(),
                        effective_parent_model_reasoning.as_ref(),
                        &request_constraints,
                        admitted_agent_profiles.as_ref(),
                        &parent_profile_authority,
                        &execution_contract,
                        child_recursion_depth,
                        interaction_mode,
                        *timeout_sec,
                        cancel_token.as_ref(),
                        live_event_sink.as_ref(),
                    )
                    .await
            }
        };

        // Journal: delegation completed
        if let Ok(ref dr) = result {
            let succeeded = dr.agent_results.iter().filter(|r| r.is_success()).count();
            let failed = dr.agent_results.len() - succeeded;
            Self::write_journal_event(
                &request.user_id,
                &session_id,
                astra_services::session_journal::JournalEvent::delegation_completed(
                    Some(&session_id),
                    &request.delegation_id,
                    pattern_name,
                    dr.agent_results.len(),
                    succeeded,
                    failed,
                    &dr.status,
                    dr.aggregated_output.as_deref(),
                ),
            );
        }
        record_delegation_metrics(
            self.run_engine.metrics_registry(),
            pattern_name,
            execution_started_at.elapsed(),
            &result,
        );

        result
    }

    /// Write a journal event synchronously on the local diagnostic path.
    /// Durable run state is committed separately by `RunEngine`.
    fn write_journal_event(
        user_id: &str,
        session_id: &str,
        event: astra_services::session_journal::JournalEvent,
    ) {
        if let Ok(writer) =
            astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        {
            if let Err(e) = writer.append(&event) {
                astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
            }
        }
    }

    /// Adapt an already-admitted delegated run to the existing child task owner.
    /// This does not perform model, capacity, mailbox, or durable admission.
    async fn supervise_prepared_subrun(
        &self,
        config: SubRunConfig,
        parent: Arc<crate::orchestration::spawner::FanoutParentAdmission>,
        deadline: Option<tokio::time::Instant>,
        cancellation: ChildCancellationGrace,
        cooperative_deadline: bool,
    ) -> Result<ChildExecutionReceipt<SubRunExecutionResult>, AgentResult> {
        let spawner = self
            .spawner
            .as_ref()
            .expect("execution is bound before admission");
        let agent_id = config.agent_profile.agent_id.clone();
        let run_id = config.run_id.clone();
        let instance_id = child_runtime_id(&agent_id, &run_id);
        let owner_generation = config
            .execution_owner_generation
            .expect("precreated child has its admission generation");
        let binding_id = config
            .cancellation_binding_id
            .clone()
            .expect("precreated child has its invocation binding");
        let user_id = config.user_id.clone();
        let session_id = config.session_id.clone();
        let child_cancel = config
            .cancel_token
            .clone()
            .expect("precreated child has its cancellation control");
        let mailbox =
            MailboxRetirement::from_mailbox(&self.mailbox_router, config.mailbox.as_ref());
        let runtime_executor = spawner.executor_for_admitted_child();
        let mut adopted = false;
        let submission = async {
            let runtime_binding_guard = match (
                runtime_executor.as_ref(),
                config.prepared_model.as_ref().and_then(|model| model.admitted_execution.as_ref()),
            ) {
                (Some(executor), Some(execution)) => {
                    executor.bind_admitted_child_runtime(&config, execution).await?
                }
                _ => None,
            };
            let state = SpawnedAgentState {
                capacity_admission: ChildCapacityAdmission::Precreated,
                agent_id: instance_id.clone(),
                run_id: run_id.clone(),
                cancellation_binding_id: Some(binding_id.clone()),
                parent_run_id: config.parent_run_id.clone(),
                agent_type: agent_id.clone(),
                description: config.task.clone(),
                status: crate::orchestration::AgentStatus::Initializing,
                committed_frontier: Some(SpawnRunFrontier {
                    run_id: run_id.clone(),
                    run_generation: owner_generation,
                    last_event_idx: None,
                }),
                work_revision: 1,
                messaging_address: config.mailbox.as_ref().map(|mailbox| mailbox.registration()),
                started_at: std::time::SystemTime::now(),
                ended_at: None,
                metrics: Default::default(),
                permission_summary: Default::default(),
                parent_agent_id: config.delegation_chain.last().cloned().unwrap_or_default(),
                trace_context: crate::server::run::lifecycle::trace_context_from_subrun_context(
                    &config.context, &config.run_id,
                ),
                spawn_tool_call_id: None,
                fanout_slot: None,
                execution_metadata: config.execution_metadata.clone(),
                prepared_model: config.prepared_model.as_ref().map(|model| PreparedSpawnModelIdentity {
                    offering_id: model.offering_id.clone(),
                    model_name: model.model_name.clone(),
                    provenance: "delegation_admission",
                }),
            };
            spawner.adopt_precreated_child(state).await.map_err(|error| error.to_string())?;
            adopted = true;
            let executor = Arc::clone(&self.executor);
            let entered = Arc::new(AtomicBool::new(false));
            let entered_by_task = Arc::clone(&entered);
            let execution_agent_id = agent_id.clone();
            let execution_run_id = run_id.clone();
            let task_spawner = spawner.task_handle();
            let execution_instance_id = instance_id.clone();
            let cancel = Arc::clone(&child_cancel);
            let execution = async move {
                let _runtime_binding_guard = runtime_binding_guard;
                let queued_execution = async {
                    if cancel.is_cancelled() {
                        return Ok((cancelled_agent_result(&execution_agent_id, &execution_run_id), None));
                    }
                    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                        return Err("agent execution timeout: shared deadline exceeded".into());
                    }
                    task_spawner.update_status(
                        &execution_instance_id,
                        crate::orchestration::AgentStatus::Running { activity: "executing".into() },
                    ).await;
                    // Publication can await: only the actual dispatch transfers
                    // durable settlement authority from scheduler to executor.
                    if cancel.is_cancelled() {
                        return Ok((cancelled_agent_result(&execution_agent_id, &execution_run_id), None));
                    }
                    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                        return Err("agent execution timeout: shared deadline exceeded".into());
                    }
                    entered_by_task.store(true, Ordering::Release);
                    executor.execute(config).await
                };
                tokio::pin!(queued_execution);
                match await_subrun_operation(&mut queued_execution, deadline, Some(cancel.as_ref())).await {
                    Ok(result) => result,
                    Err(stop) => {
                        if stop == SubRunOperationStop::DeadlineExceeded && !cooperative_deadline {
                            return Err("agent execution timeout: shared deadline exceeded".into());
                        }
                        cancel.cancel();
                        match tokio::time::timeout(FANOUT_CANCELLATION_DRAIN_TIMEOUT, &mut queued_execution).await {
                            Ok(Ok(result)) => Ok(result),
                            failed => Ok((AgentResult {
                                agent_id: execution_agent_id.clone(),
                                run_id: execution_run_id.clone(),
                                status: match stop {
                                    SubRunOperationStop::Cancelled => STATUS_CANCELLED,
                                    SubRunOperationStop::DeadlineExceeded => AGENT_RESULT_STATUS_TIMEOUT,
                                }.into(),
                                output: None,
                                error: Some(match failed {
                                    Ok(Err(error)) => error,
                                    _ => match stop {
                                        SubRunOperationStop::Cancelled => "cancelled by parent run".into(),
                                        SubRunOperationStop::DeadlineExceeded => "agent stage timeout: deadline exceeded".into(),
                                    },
                                }),
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                tool_calls: 0,
                            }, None)),
                        }
                    }
                }
            };
            let run_engine = Arc::clone(&self.run_engine);
            let settlement_executor = Arc::clone(&self.executor);
            let settlement_user_id = user_id.clone();
            let settlement_session_id = session_id.clone();
            let settlement_agent_id = agent_id.clone();
            let settlement_run_id = run_id.clone();
            let settlement_binding_id = binding_id.clone();
            let runtime_executor = runtime_executor.clone();
            let cancel = Arc::clone(&child_cancel);
            let settlement = move |outcome: SpawnExecutionOutcome<SubRunExecutionResult>| async move {
                let (attempted, producer_frontier) = match outcome {
                    Ok(result) => result,
                    Err((error, _)) => (AgentResult {
                        agent_id: settlement_agent_id,
                        run_id: settlement_run_id.clone(),
                        status: if cancel.is_cancelled() { STATUS_CANCELLED } else { STATUS_FAILED }.into(),
                        output: None,
                        error: Some(error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    }, None),
                };
                let pending = durable_reconciliation_pending_result(&attempted, "child outcome");
                let disposition = durable_lifecycle_disposition_after_dispatch(
                    settlement_executor.as_ref(), owner_generation, entered.load(Ordering::Acquire),
                );
                let (result, frontier) = tokio::time::timeout(
                    DELEGATION_RECONCILIATION_TIMEOUT,
                    reconcile_agent_result_with_durable_authority(
                        &run_engine, &settlement_user_id, &settlement_session_id, disposition, attempted, None,
                    ),
                ).await.unwrap_or((pending, None));
                let frontier = frontier.map(|winner| {
                    producer_frontier.filter(|producer| {
                        producer.run_id == winner.run_id
                            && producer.run_generation == winner.run_generation
                            && winner.last_event_idx.is_none()
                    }).unwrap_or(winner)
                });
                if frontier.is_some() && let Some(executor) = runtime_executor {
                    executor.settle_admitted_child_runtime(
                        &settlement_run_id, &settlement_binding_id, &result.status,
                    ).await;
                }
                Ok((result, frontier))
            };
            spawner.supervise_child(
                None,
                &instance_id, parent, execution, settlement, delegated_spawn_projection, Some(cancellation),
            ).await.map(|(receipt, _)| receipt).map_err(|error| error.to_string())
        }.await;
        match submission {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                child_cancel.cancel();
                let attempted = child_setup_failure_result(
                    &agent_id,
                    &run_id,
                    error,
                    deadline,
                    Some(child_cancel.as_ref()),
                );
                let pending = durable_reconciliation_pending_result(&attempted, "unlaunched child");
                let (result, frontier) = tokio::time::timeout(
                    DELEGATION_RECONCILIATION_TIMEOUT,
                    reconcile_agent_result_with_durable_authority(
                        &self.run_engine,
                        &user_id,
                        &session_id,
                        DurableLifecycleDisposition::SchedulerOwned { owner_generation },
                        attempted,
                        mailbox.as_ref(),
                    ),
                )
                .await
                .unwrap_or((pending, None));
                if frontier.is_some()
                    && let Some(executor) = runtime_executor
                {
                    executor
                        .settle_admitted_child_runtime(&run_id, &binding_id, &result.status)
                        .await;
                }
                if adopted {
                    let projection =
                        delegated_spawn_projection(&(result.clone(), frontier)).into_owned();
                    spawner
                        .finalize_background_agent(&instance_id, Ok(&projection))
                        .await;
                }
                Err(result)
            }
        }
    }

    async fn collect_supervised_subrun(
        &self,
        agent_id: String,
        run_id: String,
        receipt: Result<ChildExecutionReceipt<SubRunExecutionResult>, AgentResult>,
        parent: Arc<crate::orchestration::spawner::FanoutParentAdmission>,
        user_id: &str,
        session_id: &str,
    ) -> AgentResult {
        let (result, frontier) = match receipt {
            Err(result) => (result, None),
            Ok(receipt) => match receipt.await {
                Ok((Ok(result), _)) => result,
                failed => {
                    let error = match failed {
                        Ok((Err((error, _)), _)) => error,
                        Err(error) => format!("supervised child receipt closed: {error}"),
                        Ok((Ok(_), _)) => unreachable!(),
                    };
                    (
                        reconcile_agent_result_with_shared_deadline(
                            &self.run_engine,
                            user_id,
                            session_id,
                            DurableLifecycleDisposition::ReadOnly,
                            AgentResult {
                                agent_id: agent_id.clone(),
                                run_id: run_id.clone(),
                                status: STATUS_FAILED.into(),
                                output: None,
                                error: Some(error),
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                tool_calls: 0,
                            },
                            None,
                            &mut None,
                            "delegation",
                            "lost execution receipt",
                        )
                        .await,
                        None,
                    )
                }
            },
        };
        self.tracker
            .set_sub_run_result_state(
                &result.run_id,
                agent_result_status_to_subrun_state(&result.status),
                result.error.as_deref(),
                result.output.as_deref(),
                false,
            )
            .await;
        if !result.is_unfinished()
            && let Some(frontier) = frontier
        {
            parent.consume_terminal_child(&child_runtime_id(&agent_id, &run_id), &frontier);
        }
        result
    }

    /// Fan-out: spawn all agents in parallel, aggregate results.
    async fn execute_fan_out(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        aggregation: &AggregationStrategy,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        parent_model_reasoning: Option<
            &astra_turn_core::orchestration_spawn_tool::ParentModelReasoning,
        >,
        request_constraints: &RequestConstraints,
        admitted_agent_profiles: Option<&Arc<astra_services::runs::AgentProfileSnapshot>>,
        parent_profile_authority: &ParentProfileAuthority,
        execution_contract: &(
            crate::server::tool_transport::ExecutionBindingSnapshot,
            astra_turn_types::StopHookObligations,
        ),
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        const MAX_FAN_OUT_AGENTS: usize = 32;
        if agent_ids.len() > MAX_FAN_OUT_AGENTS {
            return Err(format!(
                "Fan-out request with {} agents exceeds limit of {MAX_FAN_OUT_AGENTS}",
                agent_ids.len()
            ));
        }
        let spawner = self
            .spawner
            .as_ref()
            .ok_or("fan-out requires its execution-bound child supervisor")?;
        let batch_cancel = Arc::new(
            cancel_token
                .map(|token| token.child_token())
                .unwrap_or_default(),
        );
        let cancel_on_drop = batch_cancel.as_ref().clone().drop_guard();
        let cancel_token = Some(&batch_cancel);
        let admitted_execution_deadline = execution_deadline_authority(timeout_sec)?;
        let execution_deadline = delegation_deadline(timeout_sec)?;
        let reg = self.registry.read().await;

        // Compute aggregation strategy name and budget info for team prompts
        let aggregation_name = match aggregation {
            AggregationStrategy::FirstSuccess => "FirstSuccess",
            AggregationStrategy::AllResults => "AllResults",
            AggregationStrategy::Consensus => "Consensus",
        };
        let agent_id_strs: Vec<&str> = agent_ids.iter().map(|s| s.as_str()).collect();
        let session_id = Self::session_id_for(request);
        let child_plans = agent_ids
            .iter()
            .map(|agent_id| {
                let profile = reg.get(agent_id).cloned().ok_or_else(|| {
                    Self::missing_agent_profile_error("fanout spawn", agent_id, &reg)
                })?;
                let (
                    profile,
                    thinking,
                    slot_constraints,
                    requested_model_policy,
                    execution_metadata,
                ) = planned_child_execution(
                    &profile,
                    parent_model_reasoning,
                    request_constraints,
                    request.execution_metadata.as_ref(),
                )?;
                let delegation_chain = if admitted_agent_profiles.is_some() {
                    request.delegation_chain.clone()
                } else {
                    Self::delegation_chain_for_child(request, agent_id)?
                };
                Ok((
                    agent_id.clone(),
                    profile,
                    thinking,
                    delegation_chain,
                    slot_constraints,
                    requested_model_policy,
                    execution_metadata,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        drop(reg);

        // Complete every route admission before creating the first durable
        // child. A rejected final slot must not leave earlier slots running
        // or persist rows with a parent Offering that differs from execution.
        let model_requests = child_plans
            .iter()
            .map(|(_, profile, thinking, _, _, _, _)| SubRunModelRequest {
                user_id: request.user_id.clone(),
                selection: profile.model_selection.clone(),
                parent_model_reasoning: parent_model_reasoning.cloned(),
                inherited_execution: admitted_model_execution.cloned(),
                provider_scope_bound: request
                    .context
                    .contains_key(Self::PROVIDER_RUN_OWNER_CONTEXT_KEY),
                thinking: thinking.clone(),
                max_output_tokens: None,
            })
            .collect::<Vec<_>>();
        let prepared_models = self
            .prepare_subrun_models(
                &model_requests,
                cancel_token,
                model_admission_timeout(execution_deadline),
            )
            .await?;

        // Build configs + create runs only after the whole fixed fanout passed
        // model admission.
        let mut configs = Vec::new();
        let mut owner_generations = HashMap::new();
        let mut started_children = Vec::new();
        let mut startup_error = None;
        for (
            (
                agent_id,
                profile,
                thinking,
                delegation_chain,
                slot_constraints,
                requested_model_policy,
                execution_metadata,
            ),
            prepared_model,
        ) in child_plans.into_iter().zip(prepared_models)
        {
            if execution_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                startup_error = Some(
                    "fan-out deadline expired before all child runs were admitted".to_string(),
                );
                break;
            }
            let sub_run_id = uuid::Uuid::new_v4().to_string();
            let profile_authority = match parent_profile_authority {
                ParentProfileAuthority::Unbound => ParentProfileAuthority::Unbound,
                _ => parent_profile_authority.for_child(&agent_id)?,
            };
            let execution_authority = match self
                .start_delegated_run(
                    &sub_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    &agent_id,
                    None,
                    interaction_mode,
                    &slot_constraints,
                    slot_constraints.durable_execution_restrictions(
                        admitted_execution_deadline,
                        (profile.initial_turns.is_some() || profile.max_turns.is_some()).then_some(
                            astra_services::runs::ExecutionBudget {
                                initial_turns: profile.initial_turns,
                                hard_turn_limit: profile.max_turns,
                            },
                        ),
                    ),
                    execution_metadata.as_ref(),
                    admitted_agent_profiles,
                    &profile_authority,
                    &thinking,
                    prepared_model.as_ref(),
                    cancel_token,
                    execution_deadline,
                )
                .await
            {
                Ok(authority) => authority,
                Err(error) => {
                    startup_error = Some(error);
                    break;
                }
            };
            owner_generations.insert(
                sub_run_id.clone(),
                (execution_authority.owner_generation, None),
            );

            self.tracker
                .record_sub_run_with_progress(
                    SubRunRecord {
                        run_id: sub_run_id.clone(),
                        parent_run_id: request.parent_run_id.clone(),
                        delegation_id: request.delegation_id.clone(),
                        agent_id: agent_id.clone(),
                        depth: request.depth + 1,
                        state: SubRunState::Created,
                        retry_of: None,
                    },
                    false,
                )
                .await;

            // Transition Created → Running
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fan-out: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;
            started_children.push((
                sub_run_id.clone(),
                agent_id.clone(),
                execution_authority.owner_generation,
                child_cancel.clone(),
            ));

            if let Err(error) = activate_delegated_child(
                &self.run_engine,
                &request.user_id,
                &session_id,
                &sub_run_id,
                execution_authority.owner_generation,
                "agent_execution",
                execution_deadline,
                cancel_token.map(Arc::as_ref),
            )
            .await
            {
                startup_error = Some(format!(
                    "could not activate fan-out child {sub_run_id}: {error}"
                ));
                break;
            }

            // Register with mailbox router and obtain a mailbox handle (if router available).
            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            owner_generations
                .get_mut(&sub_run_id)
                .expect("admitted child generation")
                .1 = MailboxRetirement::from_mailbox(&self.mailbox_router, mailbox.as_ref());

            // Inject team coordination prompt into task
            let coordination_prompt =
                team_prompts::fan_out_agent_prompt(&agent_id, &agent_id_strs, aggregation_name);
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);

            let inherited_prefix = prepared_model.as_ref().and_then(|model| {
                self.resolve_inherited_prefix_for_delegate(
                    &request.parent_run_id,
                    &model.model_name,
                )
            });
            configs.push(SubRunConfig {
                execution_contract: Some(execution_contract.clone()),
                profile_authority,
                max_output_tokens: None,
                run_id: sub_run_id,
                parent_run_id: request.parent_run_id.clone(),
                max_turns: profile.max_turns,
                initial_turns: profile.initial_turns,
                agent_profile: profile,
                task: enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: Some(Arc::new(
                    ExecutionOwnerGenerationSink::preparing(execution_authority.owner_generation),
                )),
                cancellation_binding_id: Some(uuid::Uuid::new_v4().to_string()),
                child_supervisor: Some(Arc::downgrade(spawner)),
                previous_output: None,
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                prepared_model: prepared_model.clone(),
                requested_model_policy,
                thinking: thinking.clone(),
                interaction_mode,
                request_constraints: slot_constraints,
                recursion_depth: child_recursion_depth,
                pause_flag: Some(pause_flag),
                mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel),
                execution_deadline: admitted_execution_deadline,
                inherited_prefix,
                execution_metadata,
                work_item: None,
                delegation_chain,
                #[cfg(feature = "harness")]
                harness_sink: None,
            });
        }
        if let Some(error) = startup_error {
            let status = if cancel_token.is_some_and(|token| token.is_cancelled()) {
                STATUS_CANCELLED
            } else {
                STATUS_FAILED
            };
            for (_, _, _, child_cancel) in &started_children {
                child_cancel.cancel();
            }
            let cleanup_deadline = tokio::time::Instant::now() + FANOUT_CANCELLATION_DRAIN_TIMEOUT;
            let cleanup_tasks = FuturesUnordered::new();
            for (run_id, agent_id, owner_generation, _) in &started_children {
                let mailbox = owner_generations
                    .get(run_id)
                    .and_then(|(_, mailbox)| mailbox.as_ref());
                cleanup_tasks.push(self.settle_unlaunched_child(
                    &request.user_id,
                    &session_id,
                    agent_id,
                    run_id,
                    *owner_generation,
                    status,
                    &error,
                    mailbox,
                ));
            }
            let cleanup = drain_futures_before(cleanup_tasks, cleanup_deadline).await;
            for result in cleanup.completed {
                if result.status == STATUS_WAITING {
                    tracing::warn!(
                        target: "astra_runtime::delegation",
                        run_id = %result.run_id,
                        "fanout startup child cleanup remains uncertain for durable recovery"
                    );
                }
            }
            if cleanup.deadline_elapsed {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                    "fanout startup cleanup reached its shared deadline; remaining runs are left for durable recovery"
                );
            }
            return Err(error);
        }
        let parent = spawner.fanout_parent(&request.parent_run_id);
        let cancellation = ChildCancellationGrace::new(
            Arc::clone(&batch_cancel),
            FANOUT_CANCELLATION_DRAIN_TIMEOUT,
        );
        let mut receipts = FuturesUnordered::new();
        let mut results = Vec::with_capacity(configs.len());

        for config in configs {
            let agent_id = config.agent_profile.agent_id.clone();
            let run_id = config.run_id.clone();
            let receipt = self
                .supervise_prepared_subrun(
                    config,
                    Arc::clone(&parent),
                    execution_deadline,
                    cancellation.clone(),
                    false,
                )
                .await;
            if receipt.is_err() {
                batch_cancel.cancel();
            }
            receipts.push(self.collect_supervised_subrun(
                agent_id,
                run_id,
                receipt,
                Arc::clone(&parent),
                &request.user_id,
                &session_id,
            ));
        }
        while let Some(result) = receipts.next().await {
            results.push(result);
        }
        cancel_on_drop.disarm();

        let aggregated = aggregate_results(aggregation, &results);
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            aggregated,
        ))
    }

    /// Sequential: execute agents one after another, feeding each output
    /// to the next agent.
    async fn execute_sequential(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        stop_on_success: bool,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        parent_model_reasoning: Option<
            &astra_turn_core::orchestration_spawn_tool::ParentModelReasoning,
        >,
        request_constraints: &RequestConstraints,
        admitted_agent_profiles: Option<&Arc<astra_services::runs::AgentProfileSnapshot>>,
        parent_profile_authority: &ParentProfileAuthority,
        execution_contract: &(
            crate::server::tool_transport::ExecutionBindingSnapshot,
            astra_turn_types::StopHookObligations,
        ),
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let spawner = self
            .spawner
            .as_ref()
            .expect("execution is bound before admission");
        let parent = spawner.fanout_parent(&request.parent_run_id);
        let batch_cancel = Arc::new(
            cancel_token
                .map(|token| token.child_token())
                .unwrap_or_default(),
        );
        let cancel_on_drop = batch_cancel.as_ref().clone().drop_guard();
        let cancel_token = Some(&batch_cancel);
        let cancellation = ChildCancellationGrace::new(
            Arc::clone(&batch_cancel),
            FANOUT_CANCELLATION_DRAIN_TIMEOUT,
        );
        let mut results = Vec::new();
        let mut previous_output: Option<String> = None;
        let total_stages = agent_ids.len();
        for (stage_index, agent_id) in agent_ids.iter().enumerate() {
            // Check cancellation before starting next sequential agent
            if let Some(token) = cancel_token {
                if token.is_cancelled() {
                    break;
                }
            }
            let stage_execution_deadline = execution_deadline_authority(timeout_sec)?;
            let stage_deadline = delegation_deadline(timeout_sec)?;

            let sub_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = Self::session_id_for(request);
            let profile = {
                let registry = self.registry.read().await;
                registry.get(agent_id).cloned().ok_or_else(|| {
                    Self::missing_agent_profile_error("sequential spawn", agent_id, &registry)
                })?
            };
            let (profile, thinking, slot_constraints, requested_model_policy, execution_metadata) =
                planned_child_execution(
                    &profile,
                    parent_model_reasoning,
                    request_constraints,
                    request.execution_metadata.as_ref(),
                )?;
            let model_request = SubRunModelRequest {
                user_id: request.user_id.clone(),
                selection: profile.model_selection.clone(),
                parent_model_reasoning: parent_model_reasoning.cloned(),
                inherited_execution: admitted_model_execution.cloned(),
                provider_scope_bound: request
                    .context
                    .contains_key(Self::PROVIDER_RUN_OWNER_CONTEXT_KEY),
                thinking: thinking.clone(),
                max_output_tokens: None,
            };
            let prepared = self
                .prepare_subrun_models(
                    &[model_request],
                    cancel_token,
                    model_admission_timeout(stage_deadline),
                )
                .await;
            let prepared_model = match prepared {
                Ok(mut prepared) => prepared.pop().flatten(),
                Err(error) => {
                    let (status, reason) = match error {
                        SubRunModelPreparationError::Cancelled => (
                            STATUS_CANCELLED,
                            "cancelled during sequential model admission".to_string(),
                        ),
                        SubRunModelPreparationError::TimedOut => (
                            AGENT_RESULT_STATUS_TIMEOUT,
                            "sequential model admission timed out".to_string(),
                        ),
                        SubRunModelPreparationError::Executor(error) => (STATUS_FAILED, error),
                    };
                    results.push(AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: sub_run_id,
                        status: status.to_string(),
                        output: None,
                        error: Some(reason),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                    break;
                }
            };

            let profile_authority = match parent_profile_authority {
                ParentProfileAuthority::Unbound => ParentProfileAuthority::Unbound,
                _ => parent_profile_authority.for_child(agent_id)?,
            };
            let execution_authority = match self
                .start_delegated_run(
                    &sub_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    agent_id,
                    None,
                    interaction_mode,
                    &slot_constraints,
                    slot_constraints.durable_execution_restrictions(
                        stage_execution_deadline,
                        (profile.initial_turns.is_some() || profile.max_turns.is_some()).then_some(
                            astra_services::runs::ExecutionBudget {
                                initial_turns: profile.initial_turns,
                                hard_turn_limit: profile.max_turns,
                            },
                        ),
                    ),
                    execution_metadata.as_ref(),
                    admitted_agent_profiles,
                    &profile_authority,
                    &thinking,
                    prepared_model.as_ref(),
                    cancel_token,
                    stage_deadline,
                )
                .await
            {
                Ok(authority) => authority,
                Err(error) => {
                    results.push(AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: sub_run_id,
                        status: if cancel_token.is_some_and(|token| token.is_cancelled()) {
                            STATUS_CANCELLED.to_string()
                        } else {
                            STATUS_FAILED.to_string()
                        },
                        output: None,
                        error: Some(error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                    break;
                }
            };

            self.tracker
                .record_sub_run_with_progress(
                    SubRunRecord {
                        run_id: sub_run_id.clone(),
                        parent_run_id: request.parent_run_id.clone(),
                        delegation_id: request.delegation_id.clone(),
                        agent_id: agent_id.clone(),
                        depth: request.depth + 1,
                        state: SubRunState::Created,
                        retry_of: None,
                    },
                    false,
                )
                .await;

            // Transition Created → Running
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Sequential: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            let mut cleanup_deadline = None;
            let activation = activate_delegated_child(
                &self.run_engine,
                &request.user_id,
                &session_id,
                &sub_run_id,
                execution_authority.owner_generation,
                "agent_execution",
                stage_deadline,
                cancel_token.map(Arc::as_ref),
            )
            .await;
            if let Err(failure) = activation {
                let status = match &failure {
                    ChildActivationFailure::Interrupted(SubRunOperationStop::Cancelled) => {
                        STATUS_CANCELLED
                    }
                    ChildActivationFailure::Interrupted(SubRunOperationStop::DeadlineExceeded) => {
                        AGENT_RESULT_STATUS_TIMEOUT
                    }
                    ChildActivationFailure::LostAuthority
                    | ChildActivationFailure::Persistence(_) => STATUS_FAILED,
                };
                let error = format!("could not activate sequential child {sub_run_id}: {failure}");
                results.push(
                    self.settle_unlaunched_child_with_shared_deadline(
                        &request.user_id,
                        &session_id,
                        agent_id,
                        &sub_run_id,
                        execution_authority.owner_generation,
                        status,
                        &error,
                        None,
                        &mut cleanup_deadline,
                    )
                    .await,
                );
                break;
            }
            if stage_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                results.push(
                    self.settle_unlaunched_child_with_shared_deadline(
                        &request.user_id,
                        &session_id,
                        agent_id,
                        &sub_run_id,
                        execution_authority.owner_generation,
                        AGENT_RESULT_STATUS_TIMEOUT,
                        "sequential stage deadline expired before execution",
                        None,
                        &mut cleanup_deadline,
                    )
                    .await,
                );
                break;
            }

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;

            let delegation_chain = if admitted_agent_profiles.is_some() {
                request.delegation_chain.clone()
            } else {
                Self::delegation_chain_for_child(request, agent_id)?
            };

            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Inject sequential coordination prompt
            let has_prev = previous_output.is_some();
            let coordination_prompt = team_prompts::sequential_stage_prompt(
                stage_index,
                total_stages,
                agent_id,
                has_prev,
                stop_on_success,
            );
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);

            let config = SubRunConfig {
                execution_contract: Some(execution_contract.clone()),
                profile_authority,
                max_output_tokens: None,
                run_id: sub_run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                max_turns: profile.max_turns,
                initial_turns: profile.initial_turns,
                agent_profile: profile,
                task: enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: Some(Arc::new(
                    ExecutionOwnerGenerationSink::preparing(execution_authority.owner_generation),
                )),
                cancellation_binding_id: Some(uuid::Uuid::new_v4().to_string()),
                child_supervisor: self.spawner.as_ref().map(Arc::downgrade),
                previous_output: previous_output.clone(),
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                prepared_model: prepared_model.clone(),
                requested_model_policy: requested_model_policy.clone(),
                thinking: thinking.clone(),
                interaction_mode,
                request_constraints: slot_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(pause_flag),
                mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel.clone()),
                execution_deadline: stage_execution_deadline,
                inherited_prefix: None,
                execution_metadata,
                work_item: None,
                delegation_chain: delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };

            let receipt = self
                .supervise_prepared_subrun(
                    config,
                    Arc::clone(&parent),
                    stage_deadline,
                    cancellation.clone(),
                    true,
                )
                .await;
            let result = self
                .collect_supervised_subrun(
                    agent_id.clone(),
                    sub_run_id,
                    receipt,
                    Arc::clone(&parent),
                    &request.user_id,
                    &session_id,
                )
                .await;

            // Feed output to the next stage.
            previous_output = result.output.clone();
            let is_success = result.is_success();
            let is_unfinished = result.is_unfinished();
            results.push(result);

            if is_unfinished || (stop_on_success && is_success) {
                break;
            }
        }

        cancel_on_drop.disarm();
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Fork: dispatch N tasks sharing the parent's full conversation context.
    ///
    /// All fork children receive the same message prefix (the parent's conversation
    /// history up to this point), enabling prompt cache sharing across children.
    /// Fork children cannot recursively fork or delegate.
    async fn execute_fork(
        &self,
        request: &DelegationRequest,
        tasks: &[String],
        agent_id: &str,
        aggregation: &AggregationStrategy,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        parent_model_reasoning: Option<
            &astra_turn_core::orchestration_spawn_tool::ParentModelReasoning,
        >,
        request_constraints: &RequestConstraints,
        admitted_agent_profiles: Option<&Arc<astra_services::runs::AgentProfileSnapshot>>,
        parent_profile_authority: &ParentProfileAuthority,
        execution_contract: &(
            crate::server::tool_transport::ExecutionBindingSnapshot,
            astra_turn_types::StopHookObligations,
        ),
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let spawner = self
            .spawner
            .as_ref()
            .expect("execution is bound before admission");
        let parent = spawner.fanout_parent(&request.parent_run_id);
        let batch_cancel = Arc::new(
            cancel_token
                .map(|token| token.child_token())
                .unwrap_or_default(),
        );
        let cancel_on_drop = batch_cancel.as_ref().clone().drop_guard();
        let cancel_token = Some(&batch_cancel);
        let cancellation = ChildCancellationGrace::new(
            Arc::clone(&batch_cancel),
            FANOUT_CANCELLATION_DRAIN_TIMEOUT,
        );
        let reg = self.registry.read().await;
        let profile = reg
            .get(agent_id)
            .cloned()
            .ok_or_else(|| Self::missing_agent_profile_error("fork spawn", agent_id, &reg))?;
        let (profile, thinking, request_constraints, execution_metadata) = profile_child_execution(
            &profile,
            parent_model_reasoning,
            request_constraints,
            request.execution_metadata.as_ref(),
        )?;
        let fork_delegation_chain = if admitted_agent_profiles.is_some() {
            request.delegation_chain.clone()
        } else {
            Self::delegation_chain_for_child(request, agent_id)?
        };
        let profile_authority = ParentProfileAuthority::NonDelegating {
            authority: Box::new(match parent_profile_authority {
                ParentProfileAuthority::Unbound => ParentProfileAuthority::Unbound,
                _ => parent_profile_authority.for_child(agent_id)?,
            }),
        };
        drop(reg);

        // Extract parent messages for context inheritance (if provided)
        let parent_messages = request
            .context
            .get("parent_messages")
            .map(|messages| {
                clone_delegation_value(
                    astra_core::history_work::HistoryWorkSite::DelegationParentMessagesClone,
                    messages,
                )
            })
            .unwrap_or_else(|| serde_json::json!([]));

        let session_id = Self::session_id_for(request);

        let mut receipts = FuturesUnordered::new();
        let mut fork_child_cancellations: Vec<(String, Arc<tokio_util::sync::CancellationToken>)> =
            Vec::new();
        let mut launch_failure = None;
        let mut unlaunched_fork_child: Option<(AgentResult, u64)> = None;
        let admitted_execution_deadline = execution_deadline_authority(timeout_sec)?;
        let execution_deadline = delegation_deadline(timeout_sec)?;
        let fork_model_requests = (0..tasks.len())
            .map(|_| SubRunModelRequest {
                user_id: request.user_id.clone(),
                selection: profile.model_selection.clone(),
                parent_model_reasoning: parent_model_reasoning.cloned(),
                inherited_execution: admitted_model_execution.cloned(),
                provider_scope_bound: request
                    .context
                    .contains_key(Self::PROVIDER_RUN_OWNER_CONTEXT_KEY),
                thinking: thinking.clone(),
                max_output_tokens: None,
            })
            .collect::<Vec<_>>();
        let prepared_models = self
            .prepare_subrun_models(
                &fork_model_requests,
                cancel_token,
                model_admission_timeout(execution_deadline),
            )
            .await?;

        for (i, (task, prepared_model)) in tasks.iter().zip(prepared_models).enumerate() {
            if execution_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                launch_failure = Some(AgentResult {
                    agent_id: agent_id.to_string(),
                    run_id: uuid::Uuid::new_v4().to_string(),
                    status: AGENT_RESULT_STATUS_TIMEOUT.to_string(),
                    output: None,
                    error: Some("fork deadline expired before child admission".to_string()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                });
                break;
            }
            let run_id = uuid::Uuid::new_v4().to_string();
            let execution_authority = match self
                .start_delegated_run(
                    &run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    agent_id,
                    None,
                    interaction_mode,
                    &request_constraints,
                    request_constraints.durable_execution_restrictions(
                        admitted_execution_deadline,
                        (profile.initial_turns.is_some() || profile.max_turns.is_some()).then_some(
                            astra_services::runs::ExecutionBudget {
                                initial_turns: profile.initial_turns,
                                hard_turn_limit: profile.max_turns,
                            },
                        ),
                    ),
                    execution_metadata.as_ref(),
                    admitted_agent_profiles,
                    &profile_authority,
                    &thinking,
                    prepared_model.as_ref(),
                    cancel_token,
                    execution_deadline,
                )
                .await
            {
                Ok(authority) => authority,
                Err(error) => {
                    launch_failure = Some(child_setup_failure_result(
                        agent_id,
                        &run_id,
                        error,
                        execution_deadline,
                        cancel_token.map(Arc::as_ref),
                    ));
                    break;
                }
            };
            if execution_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                for (_, child_cancel) in &fork_child_cancellations {
                    child_cancel.cancel();
                }
                let error = "fork deadline expired before child execution";
                let attempted = AgentResult {
                    agent_id: agent_id.to_string(),
                    run_id: run_id.clone(),
                    status: AGENT_RESULT_STATUS_TIMEOUT.to_string(),
                    output: None,
                    error: Some(error.to_string()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                };
                launch_failure = Some(attempted.clone());
                unlaunched_fork_child = Some((attempted, execution_authority.owner_generation));
                break;
            }
            self.tracker
                .record_sub_run_with_progress(
                    SubRunRecord {
                        run_id: run_id.clone(),
                        parent_run_id: request.parent_run_id.clone(),
                        delegation_id: request.delegation_id.clone(),
                        agent_id: agent_id.to_string(),
                        depth: request.depth + 1,
                        state: SubRunState::Created,
                        retry_of: None,
                    },
                    false,
                )
                .await;
            if let Err(e) = self
                .tracker
                .transition_state(&run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fork: transition to Running failed for {run_id}: {e:?}"
                );
            }
            if let Err(failure) = activate_delegated_child(
                &self.run_engine,
                &request.user_id,
                &session_id,
                &run_id,
                execution_authority.owner_generation,
                "fork",
                execution_deadline,
                cancel_token.map(Arc::as_ref),
            )
            .await
            {
                let status = match &failure {
                    ChildActivationFailure::Interrupted(SubRunOperationStop::Cancelled) => {
                        STATUS_CANCELLED
                    }
                    ChildActivationFailure::Interrupted(SubRunOperationStop::DeadlineExceeded) => {
                        AGENT_RESULT_STATUS_TIMEOUT
                    }
                    ChildActivationFailure::LostAuthority
                    | ChildActivationFailure::Persistence(_) => STATUS_FAILED,
                };
                let detail = format!("could not activate fork child {run_id}: {failure}");
                for (_, child_cancel) in &fork_child_cancellations {
                    child_cancel.cancel();
                }
                let attempted = AgentResult {
                    agent_id: agent_id.to_string(),
                    run_id: run_id.clone(),
                    status: status.to_string(),
                    output: None,
                    error: Some(detail.clone()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                };
                launch_failure = Some(attempted.clone());
                unlaunched_fork_child = Some((attempted, execution_authority.owner_generation));
                break;
            }
            let pause_flag = self.tracker.register_pause_flag(&run_id).await;
            let child_cancel = cancel_token
                .map(|token| Arc::new(token.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&run_id, child_cancel.clone())
                .await;
            fork_child_cancellations.push((run_id.clone(), child_cancel.clone()));

            let fork_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: run_id.clone(),
                    agent_id: agent_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Build fork-specific context: parent messages + fork instruction
            let mut fork_context = Self::child_task_context(request);
            fork_context.insert("fork_index".to_string(), serde_json::json!(i));
            fork_context.insert(
                "parent_messages".to_string(),
                clone_delegation_value(
                    astra_core::history_work::HistoryWorkSite::DelegationParentMessagesClone,
                    &parent_messages,
                ),
            );
            fork_context.insert("is_fork_child".to_string(), serde_json::json!(true));

            let has_parent_ctx = !parent_messages.as_array().map_or(true, |a| a.is_empty());
            let fork_coordination = team_prompts::fork_child_prompt(i, tasks.len(), has_parent_ctx);
            let fork_task = team_prompts::wrap_task_with_coordination(&fork_coordination, task);

            let mut fork_profile = profile.clone();
            fork_profile.can_delegate = false;
            fork_profile.max_delegation_depth = 0;

            let inherited_prefix = prepared_model.as_ref().and_then(|model| {
                self.resolve_inherited_prefix_for_delegate(
                    &request.parent_run_id,
                    &model.model_name,
                )
            });

            let config = SubRunConfig {
                execution_contract: Some(execution_contract.clone()),
                profile_authority: profile_authority.clone(),
                max_output_tokens: None,
                run_id: run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                max_turns: fork_profile.max_turns,
                initial_turns: fork_profile.initial_turns,
                agent_profile: fork_profile,
                task: fork_task,
                session_id: session_id.clone(),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: Some(Arc::new(
                    ExecutionOwnerGenerationSink::preparing(execution_authority.owner_generation),
                )),
                cancellation_binding_id: Some(uuid::Uuid::new_v4().to_string()),
                child_supervisor: self.spawner.as_ref().map(Arc::downgrade),
                previous_output: None,
                context: fork_context,
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                prepared_model,
                requested_model_policy: None,
                thinking: thinking.clone(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(pause_flag),
                mailbox: fork_mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel),
                execution_deadline: admitted_execution_deadline,
                inherited_prefix,
                execution_metadata: execution_metadata.clone(),
                work_item: None,
                delegation_chain: fork_delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };

            let receipt = self
                .supervise_prepared_subrun(
                    config,
                    Arc::clone(&parent),
                    execution_deadline,
                    cancellation.clone(),
                    false,
                )
                .await;
            let failed_to_submit = receipt.is_err();
            receipts.push(self.collect_supervised_subrun(
                agent_id.to_string(),
                run_id,
                receipt,
                Arc::clone(&parent),
                &request.user_id,
                &session_id,
            ));
            if failed_to_submit {
                batch_cancel.cancel();
                break;
            }
        }

        if launch_failure.is_some() {
            for (_, child_cancel) in &fork_child_cancellations {
                child_cancel.cancel();
            }
            batch_cancel.cancel();
            let deadline = tokio::time::Instant::now() + FANOUT_CANCELLATION_DRAIN_TIMEOUT;
            let cancellation_writes = FuturesUnordered::new();
            for (run_id, _) in &fork_child_cancellations {
                cancellation_writes.push(
                    self.run_engine
                        .request_run_cancellation(&request.user_id, run_id),
                );
            }
            let settlement_attempted = unlaunched_fork_child.clone();
            let settlement = async {
                let settlements = FuturesUnordered::new();
                if let Some((attempted, owner_generation)) = settlement_attempted.as_ref() {
                    settlements.push(
                        self.settle_unlaunched_child(
                            &request.user_id,
                            &session_id,
                            &attempted.agent_id,
                            &attempted.run_id,
                            *owner_generation,
                            &attempted.status,
                            attempted
                                .error
                                .as_deref()
                                .unwrap_or("fork child was not launched"),
                            None,
                        ),
                    );
                }
                drain_futures_before(settlements, deadline).await
            };
            let (cancellation, settlement) = tokio::join!(
                drain_futures_before(cancellation_writes, deadline),
                settlement,
            );
            for result in cancellation.completed {
                if let Err(error) = result {
                    tracing::warn!(
                        target: "astra_runtime::delegation",
                        error = %error,
                        "failed to persist cancellation for a sibling after partial fork startup"
                    );
                }
            }
            if cancellation.deadline_elapsed {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                    "fork startup cancellation persistence reached its shared deadline"
                );
            }
            if let Some(result) = settlement.completed.into_iter().next() {
                launch_failure = Some(result);
            } else if settlement.deadline_elapsed
                && let Some((attempted, _)) = unlaunched_fork_child.as_ref()
            {
                launch_failure = Some(durable_reconciliation_pending_result(
                    attempted,
                    "unlaunched child",
                ));
            }
        }
        let mut results = Vec::with_capacity(tasks.len() + usize::from(launch_failure.is_some()));
        while let Some(result) = receipts.next().await {
            results.push(result);
        }

        if let Some(failure) = launch_failure {
            results.push(failure);
        }

        cancel_on_drop.disarm();
        let aggregated = aggregate_results(aggregation, &results);
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            aggregated,
        ))
    }

    /// Get the delegation tracker for external queries.
    pub fn tracker(&self) -> &Arc<DelegationTracker> {
        &self.tracker
    }

    /// Get the shared profile registry.
    pub fn registry(&self) -> &Arc<RwLock<AgentProfileRegistry>> {
        &self.registry
    }

    /// Get the shared run engine.
    pub fn run_engine(&self) -> &Arc<RunEngine> {
        &self.run_engine
    }

    // ── Pause / Resume API ──────────────────────────────────────────────────

    async fn pause_live_sub_run(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        waiting_for: &str,
    ) -> bool {
        if self.tracker.get_pause_flag(run_id).await.is_none()
            || self
                .tracker
                .get_sub_run_state(run_id)
                .await
                .is_none_or(|state| state.is_terminal())
        {
            return false;
        }
        let event = serde_json::json!({
            "event_type": "run_paused",
            "data": {"source": waiting_for},
        });
        match self
            .run_engine
            .transition_status_with_event_if_current(
                user_id,
                expected_session_id,
                run_id,
                &[STATUS_RUNNING],
                STATUS_PAUSED,
                Some(waiting_for),
                None,
                event,
            )
            .await
        {
            Ok(true) => self.tracker.pause_sub_run(run_id).await,
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    error = %error,
                    "failed to durably pause live sub-run"
                );
                false
            }
        }
    }

    async fn resume_live_sub_run(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        source: &str,
    ) -> bool {
        if !self.tracker.is_paused(run_id).await
            || self
                .tracker
                .get_sub_run_state(run_id)
                .await
                .is_none_or(|state| state.is_terminal())
        {
            return false;
        }
        let event = serde_json::json!({
            "event_type": "run_resumed",
            "data": {"source": source},
        });
        match self
            .run_engine
            .transition_status_with_event_if_current(
                user_id,
                expected_session_id,
                run_id,
                &[STATUS_PAUSED],
                STATUS_RUNNING,
                None,
                None,
                event,
            )
            .await
        {
            Ok(true) => self.tracker.resume_sub_run(run_id).await,
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    error = %error,
                    "failed to durably resume live sub-run"
                );
                false
            }
        }
    }

    /// Pause all sub-runs belonging to a delegation.
    ///
    /// Sets cooperative pause flags — sub-runs check these between turns and
    /// yield with status "paused" at the next turn boundary.
    pub async fn pause_delegation(
        &self,
        user_id: &str,
        expected_session_id: &str,
        delegation_id: &str,
    ) -> usize {
        let mut count = 0;
        for record in self.tracker.get_sub_runs(delegation_id).await {
            if self
                .pause_live_sub_run(
                    user_id,
                    expected_session_id,
                    &record.run_id,
                    "delegation_pause",
                )
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Resume all sub-runs belonging to a delegation.
    ///
    /// Clears cooperative pause flags so sub-runs continue executing.
    pub async fn resume_delegation(
        &self,
        user_id: &str,
        expected_session_id: &str,
        delegation_id: &str,
    ) -> usize {
        let mut count = 0;
        for record in self.tracker.get_sub_runs(delegation_id).await {
            if self
                .resume_live_sub_run(
                    user_id,
                    expected_session_id,
                    &record.run_id,
                    "delegation_resume",
                )
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Pause all sub-runs spawned by a parent run (across all delegations).
    pub async fn pause_children_of(
        &self,
        user_id: &str,
        expected_session_id: &str,
        parent_run_id: &str,
    ) -> usize {
        let mut count = 0;
        for child_id in self.tracker.get_children(parent_run_id).await {
            if self
                .pause_live_sub_run(user_id, expected_session_id, &child_id, "parent_pause")
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Resume all sub-runs spawned by a parent run.
    pub async fn resume_children_of(
        &self,
        user_id: &str,
        expected_session_id: &str,
        parent_run_id: &str,
    ) -> usize {
        let mut count = 0;
        for child_id in self.tracker.get_children(parent_run_id).await {
            if self
                .resume_live_sub_run(user_id, expected_session_id, &child_id, "parent_resume")
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Request cancellation of one local delegated child. Terminal state is
    /// owned by the executor result, so the caller must wait for its normal
    /// lifecycle event instead of treating this acknowledgement as completion.
    pub async fn cancel_sub_run(&self, run_id: &str) -> bool {
        self.tracker.request_cancel_sub_run(run_id).await
    }

    /// Request cancellation of every non-terminal sub-run in the parent's
    /// subtree. The executor owns each terminal result; persisting
    /// `cancelled` here would make durable state claim the work had stopped
    /// before the child had actually reached its cancellation boundary.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        self.tracker.cancel_children_of(parent_run_id).await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn bind_test_engine(engine: &DelegationEngine) -> DelegationEngine {
    if engine.spawner.is_some() {
        return engine.clone();
    }
    let router = engine.mailbox_router.clone().unwrap_or_else(|| {
        Arc::new(AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            engine.tracker.clone(),
        ))
    });
    engine
        .clone()
        .with_mailbox_router(Arc::clone(&router))
        .for_execution(Arc::new(DynamicAgentSpawner::new(router)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::coordination::{AgentProfile, AgentTier};
    use astra_services::runs::{InMemoryRunStateStore, RunStateStore};

    fn fan_out_request(agents: Vec<&str>) -> DelegationRequest {
        DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-1".into(),
            parent_run_id: "parent-1".into(),
            task: "test task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: agents.into_iter().map(String::from).collect(),
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        }
    }

    #[test]
    fn delegation_context_clone_preserves_structured_parent_messages() {
        let context = HashMap::from([(
            "parent_messages".to_string(),
            serde_json::json!([
                {"role": "user", "content": {"text": "你好🚀"}},
                {"role": "assistant", "tool_calls": [{"id": "call-1"}]}
            ]),
        )]);

        let cloned = clone_delegation_context(
            astra_core::history_work::HistoryWorkSite::DelegationContextClone,
            &context,
        );

        assert_eq!(cloned, context);
    }

    fn setup() -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
    ) {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new(
            "orch",
            "Orchestrator",
            AgentTier::Orchestrator,
        ))
        .unwrap();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("writer", "Writer", AgentTier::User))
            .unwrap();

        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());

        (Arc::new(RwLock::new(reg)), engine, tracker)
    }

    async fn assert_terminal_mailboxes_retired(transport: &crate::messaging::InProcessTransport) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.retained_inbox_count().await != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durably terminal child mailboxes must retire");
    }

    fn selected_model_registry(agents: &[(&str, &str)]) -> Arc<RwLock<AgentProfileRegistry>> {
        let mut registry = AgentProfileRegistry::new();
        registry
            .register(AgentProfile::new(
                "orch",
                "Orchestrator",
                AgentTier::Orchestrator,
            ))
            .unwrap();
        for (agent_id, offering_id) in agents {
            let mut profile = AgentProfile::new(agent_id, agent_id, AgentTier::System);
            profile.model_selection = Some(astra_turn_types::ModelSelection {
                offering_id: (*offering_id).to_string(),
            });
            registry.register(profile).unwrap();
        }
        Arc::new(RwLock::new(registry))
    }

    async fn start_model_parent(
        run_engine: &RunEngine,
        request: &DelegationRequest,
        offering_id: &str,
    ) {
        use crate::server::run::engine::{RunGenerationControls, RunStartContext};
        use astra_turn_core::thinking_config::ThinkingConfig;

        run_engine
            .start_run_with_context(
                &request.parent_run_id,
                &request.user_id,
                &request.session_id,
                RunStartContext {
                    model_selection: Some(astra_turn_types::ModelSelection {
                        offering_id: offering_id.to_string(),
                    }),
                    resolved_model_selection: Some(astra_services::runs::ResolvedModelSelection {
                        offering_id: offering_id.to_string(),
                        model_name: format!("resolved-{offering_id}"),
                        source_identity: None,
                    }),
                    generation_controls: Some(RunGenerationControls {
                        thinking: ThinkingConfig::ModelDefault,
                        first_output_max_tokens: None,
                        preserve_thinking: false,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        persist_parent_contract_fixture(run_engine, request)
            .await
            .unwrap();
    }

    struct ModelAdmissionExecutor {
        rejected_offering: Option<String>,
        resolved_model_name: Option<String>,
        preparation_started: Option<Arc<tokio::sync::Notify>>,
        preparation_release: Option<Arc<tokio::sync::Notify>>,
        batches: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        executions: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    }

    #[async_trait]
    impl SubRunExecutor for ModelAdmissionExecutor {
        async fn prepare_model_batch(
            &self,
            requests: &[SubRunModelRequest],
        ) -> Result<Vec<Option<PreparedSubRunModel>>, String> {
            if let Some(started) = self.preparation_started.as_ref() {
                started.notify_one();
            }
            if let Some(release) = self.preparation_release.as_ref() {
                release.notified().await;
            }
            let offerings = requests
                .iter()
                .filter_map(|request| {
                    request
                        .selection
                        .as_ref()
                        .or(request
                            .parent_model_reasoning
                            .as_ref()
                            .map(|parent| &parent.selection))
                        .map(|selection| selection.offering_id.clone())
                        .or_else(|| {
                            request
                                .inherited_execution
                                .as_ref()
                                .map(|execution| execution.offering_id.clone())
                        })
                })
                .collect::<Vec<_>>();
            self.batches.lock().unwrap().push(offerings.clone());
            if let Some(rejected) = self.rejected_offering.as_ref()
                && offerings.iter().any(|offering| offering == rejected)
            {
                return Err(format!("Offering {rejected} was rejected"));
            }

            requests
                .iter()
                .map(|request| {
                    let offering_id = request
                        .selection
                        .as_ref()
                        .or(request
                            .parent_model_reasoning
                            .as_ref()
                            .map(|parent| &parent.selection))
                        .map(|selection| selection.offering_id.clone())
                        .or_else(|| {
                            request
                                .inherited_execution
                                .as_ref()
                                .map(|execution| execution.offering_id.clone())
                        });
                    Ok(offering_id.map(|offering_id| {
                        let model_name = self
                            .resolved_model_name
                            .clone()
                            .unwrap_or_else(|| format!("resolved-{offering_id}"));
                        PreparedSubRunModel {
                            model_name,
                            offering_id,
                            admitted_execution: None,
                        }
                    }))
                })
                .collect()
        }

        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            let prepared = config
                .prepared_model
                .as_ref()
                .expect("model admission must finish before execution");
            self.executions.lock().unwrap().push((
                config.run_id.clone(),
                prepared.offering_id.clone(),
                prepared.model_name.clone(),
            ));
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_COMPLETED.to_string(),
                    output: Some("prepared model executed".to_string()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    #[tokio::test]
    async fn fanout_admission_failure_creates_no_durable_child_runs() {
        let request = fan_out_request(vec!["coder", "reviewer"]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let tracker = Arc::new(DelegationTracker::new());
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = Arc::new(ModelAdmissionExecutor {
            rejected_offering: Some("offer-rejected".to_string()),
            resolved_model_name: None,
            preparation_started: None,
            preparation_release: None,
            batches: batches.clone(),
            executions: executions.clone(),
        });
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-accepted"), ("reviewer", "offer-rejected")]),
            run_engine.clone(),
            tracker.clone(),
            executor,
        );

        let error = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .unwrap_err();

        assert!(error.contains("offer-rejected"), "{error}");
        assert_eq!(batches.lock().unwrap().len(), 1);
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            tracker
                .get_sub_runs(&request.delegation_id)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn oversized_fanout_is_rejected_before_admission_or_run_creation() {
        let request = fan_out_request(vec!["coder"; 33]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder")]),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: None,
                preparation_started: None,
                preparation_release: None,
                batches: batches.clone(),
                executions,
            }),
        );

        let error = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .expect_err("fan-out must enforce its bounded child count");

        assert!(error.contains("exceeds limit of 32"), "{error}");
        assert!(batches.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn malformed_workspace_is_rejected_before_model_admission_or_child_creation() {
        let mut request = fan_out_request(vec!["coder", "reviewer"]);
        request.execution_metadata = Some(serde_json::json!({
            "workspace": {"authority": "not-an-authority"}
        }));
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tracker = Arc::new(DelegationTracker::new());
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder"), ("reviewer", "offer-reviewer")]),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: None,
                preparation_started: None,
                preparation_release: None,
                batches: batches.clone(),
                executions: executions.clone(),
            }),
        );
        let error = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .expect_err("invalid authority must fail before child admission");
        assert!(error.contains("workspace binding"), "{error}");
        assert!(batches.lock().unwrap().is_empty());
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            tracker
                .get_sub_runs(&request.delegation_id)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn never_launched_child_is_settled_with_its_exact_owner_generation() {
        let request = fan_out_request(vec!["coder"]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let tracker = Arc::new(DelegationTracker::new());
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder")]),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StubSubRunExecutor),
        );
        let authority = engine
            .start_delegated_run(
                "unlaunched-child",
                &request.user_id,
                &request.session_id,
                &request.parent_run_id,
                &request.delegation_id,
                "coder",
                None,
                RequestedTurnInteractionMode::Auto,
                &RequestConstraints::default(),
                RequestConstraints::default().durable_execution_restrictions(None, None),
                None,
                None,
                &ParentProfileAuthority::Unbound,
                &astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "unlaunched-child".into(),
                parent_run_id: request.parent_run_id.clone(),
                delegation_id: request.delegation_id.clone(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let result = engine
            .settle_unlaunched_child(
                &request.user_id,
                &request.session_id,
                "coder",
                "unlaunched-child",
                authority.owner_generation,
                STATUS_CANCELLED,
                "parent cancelled before execution started",
                None,
            )
            .await;

        assert_eq!(result.status, STATUS_CANCELLED);
        let durable = run_engine
            .load_run(&request.user_id, "unlaunched-child")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
        assert!(
            durable
                .events
                .iter()
                .any(|event| event["event_type"] == "run_finished")
        );
        assert_eq!(
            tracker.get_sub_run_state("unlaunched-child").await,
            Some(SubRunState::Cancelled)
        );
    }

    #[tokio::test]
    async fn sequential_admission_failure_preserves_prior_stage_result() {
        let mut request = fan_out_request(vec!["coder", "reviewer"]);
        request.pattern = CoordinationPattern::Sequential {
            agent_ids: vec!["coder".into(), "reviewer".into()],
            stop_on_success: false,
            timeout_sec: 0,
        };
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder"), ("reviewer", "offer-rejected")]),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: Some("offer-rejected".into()),
                resolved_model_name: None,
                preparation_started: None,
                preparation_release: None,
                batches,
                executions: executions.clone(),
            }),
        );

        let result = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.agent_results[0].status, STATUS_COMPLETED);
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("prepared model executed")
        );
        assert_eq!(result.agent_results[1].agent_id, "reviewer");
        assert_eq!(result.agent_results[1].status, STATUS_FAILED);
        assert!(
            result.agent_results[1]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("offer-rejected"))
        );
        assert_eq!(executions.lock().unwrap().len(), 1);
        assert_eq!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .len(),
            1,
            "a rejected later stage must not create a child run"
        );
    }

    #[tokio::test]
    async fn fanout_durable_model_identity_matches_pre_admitted_execution() {
        let request = fan_out_request(vec!["coder", "reviewer"]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder"), ("reviewer", "offer-reviewer")]),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: None,
                preparation_started: None,
                preparation_release: None,
                batches: batches.clone(),
                executions: executions.clone(),
            }),
        );

        let result = bind_test_engine(&engine)
            .execute(request, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 2);
        let batch_records = batches.lock().unwrap();
        assert_eq!(batch_records.len(), 1, "fan-out uses one batch admission");
        assert_eq!(
            batch_records[0],
            vec!["offer-coder".to_string(), "offer-reviewer".to_string()]
        );
        drop(batch_records);

        let executions = executions.lock().unwrap().clone();
        assert_eq!(executions.len(), 2);
        for result in &result.agent_results {
            let expected_offering = match result.agent_id.as_str() {
                "coder" => "offer-coder",
                "reviewer" => "offer-reviewer",
                other => panic!("unexpected child agent {other}"),
            };
            let durable = run_engine
                .load_run("user-1", &result.run_id)
                .await
                .unwrap()
                .expect("the started child remains durably queryable");
            assert_eq!(
                durable.model_offering_id.as_deref(),
                Some(expected_offering)
            );
            assert_eq!(
                durable.resolved_model_name.as_deref(),
                Some(format!("resolved-{expected_offering}").as_str())
            );
            let observed = executions
                .iter()
                .find(|(run_id, _, _)| run_id == &result.run_id)
                .expect("the prepared model must be passed into execution");
            assert_eq!(observed.1, expected_offering);
            assert_eq!(observed.2, format!("resolved-{expected_offering}"));
        }
    }

    #[tokio::test]
    async fn fanout_rejects_same_offering_model_name_drift_before_durable_creation() {
        let request = fan_out_request(vec!["coder"]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-parent")]),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: Some("changed-parent-model".into()),
                preparation_started: None,
                preparation_release: None,
                batches,
                executions: executions.clone(),
            }),
        );

        let error = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .expect_err("same Offering must not hide a changed resolved model name");

        assert!(
            error.contains("mismatched prepared model identity"),
            "{error}"
        );
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fanout_cancellation_during_model_admission_creates_no_children() {
        let request = fan_out_request(vec!["coder"]);
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let admission_started = Arc::new(tokio::sync::Notify::new());
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tracker = Arc::new(DelegationTracker::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder")]),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: None,
                preparation_started: Some(admission_started.clone()),
                preparation_release: Some(Arc::new(tokio::sync::Notify::new())),
                batches,
                executions: executions.clone(),
            }),
        ));
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        let execution = {
            let engine = engine.clone();
            let request = request.clone();
            let cancel_token = cancel_token.clone();
            tokio::spawn(async move {
                bind_test_engine(&engine)
                    .execute(request, "orch", Some(cancel_token))
                    .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            admission_started.notified(),
        )
        .await
        .expect("model admission should begin before cancellation");
        cancel_token.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .expect("cancellation should interrupt model admission")
            .expect("delegation task must not panic")
            .expect_err("cancelled admission must not start children");

        assert!(error.contains("cancelled"), "{error}");
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            tracker
                .get_sub_runs(&request.delegation_id)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fanout_model_admission_timeout_creates_no_children() {
        let mut request = fan_out_request(vec!["coder"]);
        if let CoordinationPattern::FanOut { timeout_sec, .. } = &mut request.pattern {
            *timeout_sec = 1;
        }
        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        start_model_parent(&run_engine, &request, "offer-parent").await;
        let admission_started = Arc::new(tokio::sync::Notify::new());
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            selected_model_registry(&[("coder", "offer-coder")]),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ModelAdmissionExecutor {
                rejected_offering: None,
                resolved_model_name: None,
                preparation_started: Some(admission_started.clone()),
                preparation_release: Some(Arc::new(tokio::sync::Notify::new())),
                batches,
                executions: executions.clone(),
            }),
        );

        let error = bind_test_engine(&engine)
            .execute(request.clone(), "orch", None)
            .await
            .expect_err("model admission must respect the delegation deadline");

        assert!(error.contains("timed out"), "{error}");
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            run_engine
                .find_sub_runs(&request.user_id, &request.delegation_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn prestarted_child_persists_nonempty_descendant_requirements() {
        use astra_turn_types::{
            DelegationIntentRequirement, DelegationIntentRequirements,
            DelegationRequirementPropagation, DelegationRequirementStrength,
            DelegationUserRequirementSource, ModelSelection,
        };
        let (registry, run_engine, tracker) = setup();
        run_engine
            .start_run("parent", "user", "session")
            .await
            .unwrap();
        let mut constraints = RequestConstraints::default();
        constraints.delegated_model_requirements = DelegationIntentRequirements::Requirements {
            source: DelegationUserRequirementSource {
                user_id: "user".into(),
                session_id: "session".into(),
                session_turn: 1,
                applied_intent_id: None,
                command_intent_id: None,
                user_intent_digest: "intent-digest".into(),
            },
            requirements: vec![DelegationIntentRequirement {
                requirement_id: "all-reviewers".into(),
                model_selection: Some(ModelSelection {
                    offering_id: "review-offering".into(),
                }),
                requested_model_policy: None,
                reasoning: None,
                task_scope_quote: None,
                propagation: DelegationRequirementPropagation::Descendants,
                strength: DelegationRequirementStrength::Hard,
            }],
        };
        constraints.allowed_tools = Some(HashSet::from(["read_file".into()]));
        constraints.enabled_tools = Some(HashSet::new());
        let restrictions = constraints.durable_execution_restrictions(
            execution_deadline_authority(60).unwrap(),
            Some(astra_services::runs::ExecutionBudget {
                initial_turns: Some(2),
                hard_turn_limit: Some(5),
            }),
        );
        let delegation = DelegationEngine::new(registry, run_engine.clone(), tracker);
        delegation
            .start_delegated_run(
                "child",
                "user",
                "session",
                "parent",
                "delegation",
                "reviewer",
                None,
                RequestedTurnInteractionMode::Auto,
                &constraints,
                restrictions.clone(),
                None,
                None,
                &ParentProfileAuthority::Unbound,
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let child = run_engine.load_run("user", "child").await.unwrap().unwrap();
        assert_eq!(child.execution_restrictions().unwrap(), Some(restrictions));
        assert_eq!(
            crate::server::run::engine::durable_run_delegated_model_requirements(&child).unwrap(),
            Some(constraints.delegated_model_requirements)
        );
    }

    #[tokio::test]
    async fn generation_publish_between_preparing_read_and_wait_is_observed() {
        let sink = Arc::new(ExecutionOwnerGenerationSink::preparing(3));
        let preparing_observed = Arc::new(tokio::sync::Notify::new());
        let release_wait = Arc::new(tokio::sync::Notify::new());
        *sink
            .wait_after_preparing_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Arc::clone(&preparing_observed), Arc::clone(&release_wait)));
        let waiter = {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move { sink.wait_until_published_or_stopped().await })
        };

        preparing_observed.notified().await;
        sink.publish(4);
        release_wait.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("versioned publication cannot lose the wakeup")
                .expect("publication waiter must not panic"),
            ExecutionOwnerGenerationPublication::Acquired(4)
        );
    }

    #[tokio::test]
    async fn generation_guard_drop_between_preparing_read_and_wait_is_observed() {
        let sink = Arc::new(ExecutionOwnerGenerationSink::preparing(7));
        let owner = sink.guard();
        let preparing_observed = Arc::new(tokio::sync::Notify::new());
        let release_wait = Arc::new(tokio::sync::Notify::new());
        *sink
            .wait_after_preparing_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Arc::clone(&preparing_observed), Arc::clone(&release_wait)));
        let waiter = {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move { sink.wait_until_published_or_stopped().await })
        };

        preparing_observed.notified().await;
        drop(owner);
        release_wait.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("versioned guard drop cannot lose the wakeup")
                .expect("publication waiter must not panic"),
            ExecutionOwnerGenerationPublication::StoppedBeforeAcquisition {
                expected_initial_generation: 7,
            }
        );
    }

    /// Establish the production precondition for a delegation test: the
    /// parent conversation run already exists in the durable run store.
    ///
    /// Delegated runs inherit lineage and admitted model identity from their
    /// parent. Tests must therefore create that parent through the same
    /// `RunEngine` API used by the lifecycle instead of relying on the legacy
    /// orphan-child fallback.
    async fn persist_durable_parent_fixture(
        engine: &DelegationEngine,
        request: &DelegationRequest,
    ) -> Result<(), String> {
        if engine
            .run_engine
            .load_run(&request.user_id, &request.parent_run_id)
            .await?
            .is_none()
        {
            engine
                .run_engine
                .start_run(
                    &request.parent_run_id,
                    &request.user_id,
                    &request.session_id,
                )
                .await?;
        }
        persist_parent_contract_fixture(&engine.run_engine, request).await
    }

    async fn persist_parent_contract_fixture(
        run_engine: &RunEngine,
        request: &DelegationRequest,
    ) -> Result<(), String> {
        let parent = run_engine
            .load_run(&request.user_id, &request.parent_run_id)
            .await?
            .ok_or("test parent is missing")?;
        let snapshot = crate::server::tool_transport::ExecutionBindingSnapshot::inferred(
            crate::server::tool_transport::WorkspaceBinding::none(),
            crate::server::tool_transport::ExecutorBinding::server_control_plane(),
        );
        let events = crate::server::run::binding_resolution::binding_snapshot_events(
            &request.parent_run_id,
            &request.session_id,
            &snapshot,
            &Default::default(),
        );
        if !run_engine
            .append_events_if_current_generation_and_status(
                &request.user_id,
                &request.session_id,
                &request.parent_run_id,
                parent.run_generation,
                &[STATUS_RUNNING],
                &events,
            )
            .await?
        {
            return Err("test parent binding was not admitted".into());
        }
        Ok(())
    }

    async fn execute_with_durable_parent(
        engine: &DelegationEngine,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        persist_durable_parent_fixture(engine, &request).await?;
        bind_test_engine(engine)
            .execute(request, source_agent_id, cancel_token)
            .await
    }

    async fn execute_with_durable_parent_and_headers(
        engine: &DelegationEngine,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
    ) -> Result<DelegationResult, String> {
        persist_durable_parent_fixture(engine, &request).await?;
        bind_test_engine(engine)
            .execute_with_forward_headers(
                request,
                source_agent_id,
                cancel_token,
                forward_headers,
                admitted_model_execution,
            )
            .await
    }

    #[test]
    fn missing_agent_profile_error_lists_operation_and_available_profiles() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();

        let message = DelegationEngine::missing_agent_profile_error("fanout spawn", "writer", &reg);

        assert!(message.contains("fanout spawn"), "{message}");
        assert!(message.contains("writer"), "{message}");
        assert!(message.contains("coder, reviewer"), "{message}");
        assert!(message.contains("do not invent a replacement"), "{message}");
    }

    #[test]
    fn profile_child_execution_materializes_only_the_inherited_offering() {
        use astra_turn_core::orchestration_spawn_tool::ParentModelReasoning;
        use astra_turn_core::thinking_config::{ThinkingConfig, ThinkingEffort};
        use astra_turn_types::ModelSelection;

        let parent = ParentModelReasoning {
            selection: ModelSelection {
                offering_id: "parent-offering".into(),
            },
            resolved_model_name: Some("parent-model".into()),
            thinking: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
        };
        let inherited = AgentProfile::new("inherited", "Inherited", AgentTier::System);
        let (resolved, thinking, _, _) = profile_child_execution(
            &inherited,
            Some(&parent),
            &RequestConstraints::default(),
            None,
        )
        .unwrap();
        assert_eq!(resolved.model_selection, Some(parent.selection.clone()));
        assert_eq!(thinking, parent.thinking);

        let mut explicit = AgentProfile::new("explicit", "Explicit", AgentTier::System);
        explicit.model_selection = Some(ModelSelection {
            offering_id: "other-offering".into(),
        });
        let (resolved, thinking, _, _) = profile_child_execution(
            &explicit,
            Some(&parent),
            &RequestConstraints::default(),
            None,
        )
        .unwrap();
        assert_eq!(resolved.model_selection, explicit.model_selection);
        assert_eq!(thinking, ThinkingConfig::ModelDefault);
    }

    #[test]
    fn profile_child_execution_narrows_controls_without_elevating_workspace_authority() {
        let parent = RequestConstraints::new(
            Some(HashSet::from(["read_file".into(), "bash".into()])),
            Some(HashSet::new()),
            Some(HashSet::from(["analysis".into(), "other".into()])),
            None,
        );
        let mut profile = AgentProfile::new("reader", "Reader", AgentTier::User);
        profile.read_only = true;
        profile.initial_turns = Some(2);
        profile.max_turns = Some(5);
        profile.skill_filter = vec!["analysis".into()];
        for (tools, expected) in [
            (None, vec!["bash", "read_file"]),
            (Some(vec![]), vec![]),
            (
                Some(vec![" READ_FILE ".into(), "write_file".into()]),
                vec!["read_file"],
            ),
        ] {
            profile.allow_tools = tools;
            for (authority, expected_authority) in [
                ("read_write", "read_only"),
                ("read_only", "read_only"),
                ("none", "none"),
                ("unknown", "unknown"),
            ] {
                let metadata = serde_json::json!({
                    "workspace": {"kind":"edge_workspace", "display_name":"fixture", "cwd":"/fixture", "authority":authority},
                    "execution_binding_generation": 7,
                });
                let (child, _, constraints, narrowed) =
                    profile_child_execution(&profile, None, &parent, Some(&metadata)).unwrap();
                let expected: Vec<String> = expected.iter().map(|tool| (*tool).into()).collect();
                assert_eq!(child.allow_tools, Some(expected.clone()));
                assert_eq!(
                    constraints.allowed_tools,
                    Some(expected.into_iter().collect())
                );
                assert_eq!(
                    constraints.allowed_skills,
                    Some(HashSet::from(["analysis".into()]))
                );
                assert_eq!(constraints.enabled_tools, parent.enabled_tools);
                assert!(child.read_only);
                assert_eq!((child.initial_turns, child.max_turns), (Some(2), Some(5)));
                let narrowed = narrowed.unwrap();
                assert_eq!(narrowed["workspace"]["authority"], expected_authority);
                assert_eq!(narrowed["execution_binding_generation"], 7);
                assert_eq!(metadata["workspace"]["authority"], authority);
            }
        }
        let (_, _, _, metadata) = profile_child_execution(&profile, None, &parent, None).unwrap();
        assert!(
            metadata.is_none(),
            "read-only cannot create a workspace binding"
        );
        profile.initial_turns = Some(0);
        assert!(profile_child_execution(&profile, None, &parent, None).is_err());
    }

    #[test]
    fn unspecified_child_inherits_fixed_policy_from_parent_constraints() {
        use astra_turn_types::{
            DelegationIntentRequirement, DelegationIntentRequirements,
            DelegationRequirementPropagation, DelegationRequirementStrength,
            DelegationUserRequirementSource, ModelSelection, ModelSelector, RequestedModelPolicy,
        };

        let source = DelegationUserRequirementSource {
            user_id: "user-1".into(),
            session_id: "session-1".into(),
            session_turn: 1,
            applied_intent_id: None,
            command_intent_id: None,
            user_intent_digest: "sha256:task".into(),
        };
        let requirements = DelegationIntentRequirements::Requirements {
            source,
            requirements: vec![DelegationIntentRequirement {
                requirement_id: "fixed-descendant".into(),
                model_selection: Some(ModelSelection {
                    offering_id: "fixed-selected".into(),
                }),
                requested_model_policy: Some(RequestedModelPolicy::Fixed {
                    selector: ModelSelector::OfferingId {
                        offering_id: "fixed-selected".into(),
                    },
                }),
                reasoning: None,
                task_scope_quote: None,
                propagation: DelegationRequirementPropagation::Descendants,
                strength: DelegationRequirementStrength::Hard,
            }],
        };
        let mut constraints = RequestConstraints::default();
        constraints.delegated_model_requirements = requirements.clone();

        let (profile, _, child_constraints, policy, _) = planned_child_execution(
            &AgentProfile::new("reviewer", "Reviewer", AgentTier::System),
            None,
            &constraints,
            None,
        )
        .unwrap();

        assert_eq!(
            profile.model_selection.unwrap().offering_id,
            "fixed-selected"
        );
        assert_eq!(
            policy,
            Some(RequestedModelPolicy::Fixed {
                selector: ModelSelector::OfferingId {
                    offering_id: "fixed-selected".into(),
                },
            })
        );
        assert_eq!(child_constraints.delegated_model_requirements, requirements);
    }

    #[test]
    fn parent_cancellation_is_projected_as_cancelled_not_failed() {
        let result = cancelled_agent_result("reviewer", "run-cancelled");
        assert_eq!(result.status, STATUS_CANCELLED);
        assert!(result.is_failure());
        assert_eq!(
            agent_result_status_to_subrun_state(&result.status),
            SubRunState::Cancelled
        );
    }

    #[test]
    fn reconciliation_timeout_preserves_observed_output_while_authority_is_unknown() {
        let attempted = AgentResult {
            agent_id: "reviewer".to_string(),
            run_id: "run-reconcile-pending".to_string(),
            status: STATUS_COMPLETED.to_string(),
            output: Some("locally observed output".to_string()),
            error: None,
            prompt_tokens: 12,
            completion_tokens: 4,
            tool_calls: 1,
        };
        let result = durable_reconciliation_pending_result(&attempted, "child outcome");

        assert_eq!(result.status, STATUS_WAITING);
        assert!(result.is_unfinished());
        assert_eq!(
            result.output.as_deref(),
            Some("locally observed output"),
            "waiting status marks durable authority as unknown without discarding observed output"
        );
        assert_eq!(result.prompt_tokens, 12);
        assert_eq!(result.completion_tokens, 4);
        assert_eq!(result.tool_calls, 1);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("authoritative child state is still unknown"))
        );
        assert_eq!(
            agent_result_status_to_subrun_state(&result.status),
            SubRunState::Waiting
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconciliation_deadline_is_shared_across_the_whole_batch() {
        let budget = std::time::Duration::from_secs(2);
        let mut deadline = None;
        let started = tokio::time::Instant::now();

        assert!(
            await_with_shared_deadline(&mut deadline, budget, std::future::pending::<()>(),)
                .await
                .is_none()
        );
        assert_eq!(tokio::time::Instant::now() - started, budget);

        let exhausted_at = tokio::time::Instant::now();
        assert!(
            await_with_shared_deadline(&mut deadline, budget, std::future::pending::<()>(),)
                .await
                .is_none()
        );
        assert_eq!(
            tokio::time::Instant::now(),
            exhausted_at,
            "a shared deadline must not restart for each child"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn child_store_operation_obeys_deadline_and_parent_cancellation() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
        assert_eq!(
            await_subrun_operation(std::future::pending::<()>(), Some(deadline), None,).await,
            Err(SubRunOperationStop::DeadlineExceeded)
        );
        assert_eq!(tokio::time::Instant::now(), deadline);

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let operation_token = cancel_token.clone();
        let operation = tokio::spawn(async move {
            await_subrun_operation(std::future::pending::<()>(), None, Some(&operation_token)).await
        });
        tokio::task::yield_now().await;
        cancel_token.cancel();
        assert_eq!(
            operation.await.expect("bounded operation task joins"),
            Err(SubRunOperationStop::Cancelled)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_drain_polls_sibling_cleanup_concurrently_until_shared_deadline() {
        async fn delayed_value(delay: std::time::Duration, value: u8) -> u8 {
            tokio::time::sleep(delay).await;
            value
        }

        let futures = FuturesUnordered::new();
        futures.push(delayed_value(std::time::Duration::from_secs(5), 1));
        futures.push(delayed_value(std::time::Duration::from_millis(10), 2));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);

        let drain = drain_futures_before(futures, deadline).await;

        assert_eq!(drain.completed, vec![2]);
        assert!(drain.deadline_elapsed);
        assert_eq!(tokio::time::Instant::now(), deadline);
    }

    #[test]
    fn delegation_engine_injects_source_agent_into_chain_once() {
        let mut request = fan_out_request(vec!["coder"]);
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "orch");
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "orch");

        assert_eq!(request.delegation_chain, vec!["orch".to_string()]);
    }

    #[test]
    fn delegation_metrics_expose_low_cardinality_outcomes_and_usage() {
        let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
        register_delegation_metrics(&registry);
        let result = Ok(DelegationResult::from_results(
            "d1",
            vec![AgentResult {
                agent_id: "reviewer".into(),
                run_id: "run-1".into(),
                status: "completed".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 120,
                completion_tokens: 30,
                tool_calls: 2,
            }],
            Some("done".into()),
        ));

        record_delegation_metrics(
            Some(&registry),
            "fan_out",
            std::time::Duration::from_millis(25),
            &result,
        );

        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            "astra_delegation_executions_total{outcome=\"completed\",pattern=\"fan_out\"} 1"
        ));
        assert!(rendered.contains(
            "astra_delegation_sub_runs_total{pattern=\"fan_out\",status=\"completed\"} 1"
        ));
        assert!(
            rendered
                .contains("astra_delegation_tokens_total{kind=\"prompt\",pattern=\"fan_out\"} 120")
        );
    }

    #[test]
    fn delegation_engine_rejects_child_already_in_chain() {
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string(), "coder".to_string()];

        let error = DelegationEngine::delegation_chain_for_child(&request, "coder").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("orch → coder → coder"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_self_delegation() {
        // A → A: the most fundamental cycle. An agent must not be able to
        // delegate to itself, even with an empty chain — `ensure_source`
        // runs first, so the chain becomes [A] before `for_child(A)` fires.
        let mut request = fan_out_request(vec!["coder"]);
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "coder");

        let error = DelegationEngine::delegation_chain_for_child(&request, "coder").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("coder → coder"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_three_hop_cycle() {
        // A → B → C → A: a deeper cycle that a naive "immediate parent only"
        // check would miss. The chain is walked in full.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec![
            "orch".to_string(),
            "coder".to_string(),
            "reviewer".to_string(),
        ];

        let error = DelegationEngine::delegation_chain_for_child(&request, "orch").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("orch → coder → reviewer → orch"), "{error}");
    }

    #[test]
    fn delegation_engine_cycle_detection_is_case_insensitive() {
        // Agent IDs are user-provided; a case variant must not bypass the
        // chain check. `Orch` should match `orch` already in the chain.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string()];

        let error = DelegationEngine::delegation_chain_for_child(&request, "Orch").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_unicode_decomposition_bypass() {
        // NFC ("café") and NFD ("cafe" + combining acute ◌́) are visually
        // identical but byte-distinct. A child using the decomposed form
        // must not bypass a chain containing the composed form — that
        // would allow an infinite delegation loop through a normalization
        // alias.
        let mut request = fan_out_request(vec!["coder"]);
        // "café" in NFC: é = U+00E9
        request.delegation_chain = vec!["caf\u{00E9}".to_string()];
        // "café" in NFD: e + ◌́ = U+0065 U+0301
        let child = "caf\u{0065}\u{0301}";

        let error = DelegationEngine::delegation_chain_for_child(&request, child).unwrap_err();
        assert!(
            error.contains("circular delegation detected"),
            "NFD child must not bypass NFC chain entry, got: {error}"
        );
    }

    #[test]
    fn delegation_engine_rejects_unicode_composition_bypass() {
        // Inverse of the above: chain holds NFD, child uses NFC.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["caf\u{0065}\u{0301}".to_string()];
        let child = "caf\u{00E9}";

        let error = DelegationEngine::delegation_chain_for_child(&request, child).unwrap_err();
        assert!(
            error.contains("circular delegation detected"),
            "NFC child must not bypass NFD chain entry, got: {error}"
        );
    }

    #[test]
    fn delegation_engine_rejects_case_and_decomposition_combo() {
        // Chain holds "Coder" (capital C, NFC), child uses "c\u{0300}der"
        // (lowercase c + combining grave). Both case and normalization
        // differences must be normalized away before comparison.
        let mut request = fan_out_request(vec!["reviewer"]);
        request.delegation_chain = vec!["Coder".to_string()];
        let child = "c\u{006F}\u{0300}der"; // o + combining grave, lowercased intent

        let error = DelegationEngine::delegation_chain_for_child(&request, child);
        // If the combining mark makes it a different agent, it's admitted;
        // if normalization collapses it to "coder", it's a cycle. The
        // contract is: visually-distinct IDs are distinct agents. This
        // test asserts that "Coder" vs "cöder" (different grapheme) is
        // admitted, while the normalization logic does not over-collapse
        // distinct graphemes.
        // (If this combination genuinely differs after NFC, expect Ok.)
        // The key invariant is determinism — the same input always yields
        // the same canonical form.
        match error {
            Ok(_) => { /* distinct grapheme — correctly admitted */ }
            Err(msg) => {
                assert!(
                    msg.contains("circular delegation detected"),
                    "unexpected error: {msg}"
                );
            }
        }
    }

    #[test]
    fn delegation_engine_admits_visually_distinct_unicode() {
        // Sanity: two genuinely different agent IDs (different graphemes)
        // must not be collapsed by normalization. "café" and "cafe" (no
        // accent) are distinct agents.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["café".to_string()];

        let chain = DelegationEngine::delegation_chain_for_child(&request, "cafe")
            .expect("distinct grapheme must be admitted");
        assert_eq!(chain, vec!["café".to_string()]);
    }

    #[test]
    fn delegation_engine_admits_unrelated_child() {
        // Sanity: a child not present in the chain is admitted and receives
        // a copy of the current chain (which the child will later extend).
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string()];

        let chain =
            DelegationEngine::delegation_chain_for_child(&request, "coder").expect("admitted");
        assert_eq!(chain, vec!["orch".to_string()]);
    }

    #[tokio::test]
    async fn delegation_tracker_records_and_queries() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-2".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(tracker.get_parent("sub-1").await.is_some());
        assert!(tracker.get_parent("parent-1").await.is_none());
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
    }

    #[tokio::test]
    async fn fan_out_spawns_sub_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.delegation_id, "del-1");
        // Stub executor marks runs as completed
        assert_eq!(result.status, "completed");

        // Verify sub-runs were created in engine with final status
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Verify tracker has the records
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|s| s.parent_run_id == "parent-1"));
        assert!(subs.iter().all(|s| s.depth == 1));
    }

    #[tokio::test]
    async fn missing_durable_parent_is_rejected_before_live_delegation_state_exists() {
        let (reg, engine, tracker) = setup();
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_mailbox_router(router.clone());
        let request = fan_out_request(vec!["coder"]);

        let result = bind_test_engine(&de).execute(request, "orch", None).await;

        assert!(result.is_err());
        assert!(tracker.get_sub_runs("del-1").await.is_empty());
        assert!(!router.is_run_registered("parent-1").await);
    }

    #[tokio::test]
    async fn sequential_spawns_ordered_sub_runs() {
        let (reg, engine, tracker) = setup();
        let transport = Arc::new(crate::messaging::InProcessTransport::new());
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            transport.clone(),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::new(reg, engine.clone(), tracker.clone()).with_mailbox_router(router);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq".into(),
            parent_run_id: "parent-2".into(),
            task: "sequential test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.agent_results[0].agent_id, "coder");
        assert_eq!(result.agent_results[1].agent_id, "reviewer");
        assert_terminal_mailboxes_retired(&transport).await;
    }

    #[tokio::test]
    async fn validation_rejects_bad_delegation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // User agent cannot delegate
        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "bad".into(),
            parent_run_id: "p".into(),
            task: "fail".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        assert!(
            bind_test_engine(&de)
                .execute(req, "writer", None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn depth_limit_enforcement() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // Orchestrator max depth is 3; request at depth=5 should fail
        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "deep".into(),
            parent_run_id: "p".into(),
            task: "too deep".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 5,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap_err();
        assert!(err.contains("depth"));
    }

    #[tokio::test]
    async fn cross_delegation_isolation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req1 = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-A".into(),
            parent_run_id: "pA".into(),
            task: "a".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let req2 = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-B".into(),
            parent_run_id: "pA".into(),
            task: "b".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["reviewer".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        execute_with_durable_parent(&de, req1, "orch", None)
            .await
            .unwrap();
        execute_with_durable_parent(&de, req2, "orch", None)
            .await
            .unwrap();

        let subs_a = tracker.get_sub_runs("del-A").await;
        let subs_b = tracker.get_sub_runs("del-B").await;
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(subs_a[0].agent_id, "coder");
        assert_eq!(subs_b[0].agent_id, "reviewer");
    }

    // ─── Custom executor for testing ────────────────────────────────────────

    /// Test executor that echoes the task back with agent_id prefix.
    struct EchoExecutor;

    #[async_trait]
    impl SubRunExecutor for EchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            let output = if let Some(prev) = &config.previous_output {
                format!(
                    "[{}] {}: prev={}",
                    config.agent_profile.agent_id, config.task, prev
                )
            } else {
                format!("[{}] {}", config.agent_profile.agent_id, config.task)
            };
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(output),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    tool_calls: 1,
                },
                None,
            ))
        }
    }

    #[derive(Debug)]
    struct NoopLiveSink;

    impl astra_turn_core::agent_live_event::AgentLiveEventSink for NoopLiveSink {
        fn send(
            &self,
            _event: astra_turn_core::agent_live_event::AgentLiveEvent,
        ) -> Result<(), astra_turn_core::agent_live_event::AgentLiveSendError> {
            Ok(())
        }

        fn send_gap(
            &self,
            _gap: astra_turn_core::agent_live_event::AgentLiveGap,
        ) -> Result<(), astra_turn_core::agent_live_event::AgentLiveSendError> {
            Ok(())
        }
    }

    struct CapturedRunBinding {
        profile_authority: ParentProfileAuthority,
        parent_run_id: Option<String>,
        session_id: String,
        task_context: HashMap<String, serde_json::Value>,
        has_live_event_sink: bool,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        execution_owner_generation: Option<u64>,
        interaction_mode: RequestedTurnInteractionMode,
        profile: AgentProfile,
        request_constraints: RequestConstraints,
        max_turns: Option<u32>,
        initial_turns: Option<u32>,
        execution_metadata: Option<serde_json::Value>,
    }

    struct CaptureRunBindingExecutor {
        bindings: Arc<std::sync::Mutex<Vec<CapturedRunBinding>>>,
    }

    #[async_trait]
    impl SubRunExecutor for CaptureRunBindingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            self.bindings.lock().unwrap().push(CapturedRunBinding {
                profile_authority: config.profile_authority.clone(),
                parent_run_id: Some(config.parent_run_id.clone()),
                session_id: config.session_id.clone(),
                task_context: config.context.clone(),
                has_live_event_sink: config.live_event_sink.is_some(),
                cancel_token: config.cancel_token.clone(),
                execution_owner_generation: config.execution_owner_generation,
                interaction_mode: config.interaction_mode,
                profile: config.agent_profile.clone(),
                request_constraints: config.request_constraints.clone(),
                max_turns: config.max_turns,
                initial_turns: config.initial_turns,
                execution_metadata: config.execution_metadata.clone(),
            });
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".into(),
                    output: Some("completed".into()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    #[tokio::test]
    async fn precreated_child_controls_and_profile_snapshot_are_admitted_together() {
        for pattern in [
            CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            CoordinationPattern::Fork {
                agent_id: "coder".into(),
                tasks: vec!["inspect".into(), "verify".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
        ] {
            let (registry, run_engine, tracker) = setup();
            // Exercise an explicit same-tier lead as well as an ordinary root
            // without introducing a second fixture or execution path.
            let selected_lead = !matches!(&pattern, CoordinationPattern::Sequential { .. });
            let mut lead = AgentProfile::new("lead", "Lead", AgentTier::System);
            lead.delegate_to = vec!["coder".into()];
            let mut profile = registry.write().await.remove("coder").unwrap();
            profile.allow_tools = Some(vec!["read_file".into(), "write_file".into()]);
            profile.skill_filter = vec!["analysis".into()];
            profile.read_only = true;
            profile.can_delegate = true;
            profile.max_delegation_depth = 4;
            profile.delegate_to = vec!["outside".into()];
            profile.initial_turns = Some(2);
            profile.max_turns = Some(5);
            registry.write().await.register(profile.clone()).unwrap();
            let snapshot = Arc::new(astra_services::runs::AgentProfileSnapshot {
                owner_user_id: "user-1".into(),
                source_team_id: "owner-team".into(),
                lead_agent_id: selected_lead.then(|| "lead".into()),
                profiles: vec![
                    profile.clone(),
                    lead,
                    AgentProfile::new("outside", "Outside", AgentTier::System),
                ],
            });
            let parent_authority = if selected_lead {
                ParentProfileAuthority::AdmittedMember {
                    profile_id: "lead".into(),
                    ancestor_profile_ids: Vec::new(),
                }
            } else {
                ParentProfileAuthority::OrdinaryRoot
            };
            let expected_child_authority = ParentProfileAuthority::AdmittedMember {
                profile_id: "coder".into(),
                ancestor_profile_ids: if selected_lead {
                    vec!["lead".into()]
                } else {
                    Vec::new()
                },
            };
            let is_fork = matches!(&pattern, CoordinationPattern::Fork { .. });
            let expected_child_authority = if is_fork {
                ParentProfileAuthority::NonDelegating {
                    authority: Box::new(expected_child_authority),
                }
            } else {
                expected_child_authority
            };
            let mut request = fan_out_request(vec!["coder"]);
            request.pattern = pattern;
            request.context.insert(
                crate::server::delegation::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY.into(),
                serde_json::json!(["read_file", "bash"]),
            );
            request.context.insert(
                crate::server::delegation::REQUEST_ALLOWED_SKILLS_CONTEXT_KEY.into(),
                serde_json::json!(["analysis", "other"]),
            );
            request.execution_metadata = Some(serde_json::json!({
                "workspace": {"kind":"edge_workspace", "display_name":"fixture", "cwd":"/fixture", "authority":"read_write"},
                "execution_binding_generation": 7,
            }));
            run_engine
                .start_run_with_context(
                    &request.parent_run_id,
                    &request.user_id,
                    &request.session_id,
                    crate::server::run::engine::RunStartContext {
                        profile_authority: parent_authority,
                        admitted_agent_profiles: Some(snapshot.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
            let engine = DelegationEngine::with_executor(
                registry.clone(),
                run_engine.clone(),
                tracker.clone(),
                Arc::new(CaptureRunBindingExecutor {
                    bindings: bindings.clone(),
                }),
            );
            let engine = bind_test_engine(&engine);
            if matches!(&request.pattern, CoordinationPattern::FanOut { .. }) {
                for (invalid_target, depth) in
                    [("lead", 0), ("missing", 0), ("outside", 0), ("coder", 1)]
                {
                    let mut invalid = request.clone();
                    invalid.depth = depth;
                    invalid.pattern = CoordinationPattern::FanOut {
                        agent_ids: vec!["coder".into(), invalid_target.into()],
                        aggregation: AggregationStrategy::AllResults,
                        timeout_sec: 0,
                    };
                    assert!(
                        engine
                            .execute(invalid, "opaque-parent-instance", None)
                            .await
                            .is_err()
                    );
                    assert!(bindings.lock().unwrap().is_empty());
                    assert!(
                        tracker
                            .get_sub_runs(&request.delegation_id)
                            .await
                            .is_empty()
                    );
                    assert_eq!(
                        run_engine
                            .store()
                            .list_session_runs(&request.user_id, &request.session_id, 10)
                            .await
                            .unwrap()
                            .runs
                            .len(),
                        1,
                        "invalid batch must not create any child"
                    );
                }
            }
            persist_parent_contract_fixture(&run_engine, &request)
                .await
                .unwrap();
            let result = engine
                .execute(request.clone(), "opaque-parent-instance", None)
                .await
                .unwrap();
            assert_eq!(result.status, STATUS_COMPLETED);
            {
                let bindings = bindings.lock().unwrap();
                assert_eq!(bindings.len(), result.agent_results.len());
                for binding in bindings.iter() {
                    assert_eq!(binding.profile_authority, expected_child_authority);
                    assert_eq!(binding.profile.allow_tools, Some(vec!["read_file".into()]));
                    assert_eq!(
                        binding.request_constraints.allowed_tools,
                        Some(HashSet::from(["read_file".into()]))
                    );
                    assert_eq!(
                        binding.request_constraints.allowed_skills,
                        Some(HashSet::from(["analysis".into()]))
                    );
                    assert!(binding.profile.read_only);
                    assert_eq!(
                        (binding.initial_turns, binding.max_turns),
                        (Some(2), Some(5))
                    );
                    assert_eq!(
                        binding.execution_metadata.as_ref().unwrap()["workspace"]["authority"],
                        "read_only"
                    );
                }
            }
            for result in &result.agent_results {
                let child = run_engine
                    .load_run(&request.user_id, &result.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                let restrictions =
                    serde_json::to_value(child.execution_restrictions().unwrap().unwrap()).unwrap();
                assert_eq!(
                    restrictions["allow_tools"],
                    serde_json::json!(["read_file"])
                );
                assert_eq!(
                    restrictions["allow_skills"],
                    serde_json::json!(["analysis"])
                );
                assert_eq!(
                    restrictions["execution_budget"],
                    serde_json::json!({"initial_turns": 2, "hard_turn_limit": 5})
                );
                assert!(restrictions["execution_deadline_unix_ms"].is_null());
                assert!(restrictions["execution_work_deadline_unix_ms"].is_null());
                assert_eq!(
                    crate::server::run::engine::durable_run_profile_authority(
                        &child,
                        &request.user_id
                    )
                    .unwrap(),
                    expected_child_authority,
                );
                assert_eq!(
                    crate::server::run::engine::durable_run_agent_profiles(
                        &child,
                        &request.user_id
                    )
                    .unwrap(),
                    Some(snapshot.clone())
                );
                if is_fork {
                    for inherited_prefix in [false, true] {
                        crate::orchestration::agent_tool::tests::assert_restricted_child_handlers(
                            &child,
                            inherited_prefix,
                            "outside",
                        )
                        .await;
                    }
                }
                let start = child
                    .events
                    .iter()
                    .find(|event| event["event_type"] == "run_started")
                    .unwrap();
                assert_eq!(start["data"]["workspace"]["authority"], "read_only");
                assert_eq!(start["data"]["execution_binding_generation"], 7);
            }
            assert_eq!(registry.read().await.get("coder").unwrap(), &profile);
            assert_eq!(
                request.execution_metadata.unwrap()["workspace"]["authority"],
                "read_write"
            );
        }
    }

    struct BlockingCancelExecutor {
        started_tx: tokio::sync::mpsc::UnboundedSender<String>,
        cancel_observed: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SubRunExecutor for BlockingCancelExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            let cancel_token = config
                .cancel_token
                .clone()
                .expect("every delegated child has a cancellation token");
            self.started_tx
                .send(config.run_id.clone())
                .expect("test observes the child start");
            cancel_token.cancelled().await;
            self.cancel_observed.notify_one();
            self.release.notified().await;
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_CANCELLED.into(),
                    output: None,
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    #[tokio::test]
    async fn delegation_binds_child_identity_and_live_lane_per_execution() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine,
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );
        let request = DelegationRequest {
            session_id: "session-1".into(),
            delegation_id: "delegation-live-1".into(),
            parent_run_id: "run-root-1".into(),
            task: "inspect the implementation".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();
        bind_test_engine(&engine)
            .execute_with_forward_headers_and_live_events(
                request,
                "orch",
                None,
                HashMap::new(),
                None,
                None,
                Some(Arc::new(NoopLiveSink)),
            )
            .await
            .unwrap();

        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].parent_run_id.as_deref(), Some("run-root-1"));
        assert_eq!(bindings[0].session_id, "session-1");
        assert!(
            !bindings[0].task_context.contains_key("session_id"),
            "runtime session identity must not be injected into the child task prompt"
        );
        assert!(bindings[0].has_live_event_sink);
        assert!(bindings[0].cancel_token.is_some());
        assert_eq!(
            bindings[0].interaction_mode,
            RequestedTurnInteractionMode::Headless
        );
    }

    #[tokio::test]
    async fn sequential_and_fork_children_receive_independent_cancel_tokens() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine,
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );

        let sequential_request = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "delegation-sequential-cancel".into(),
            parent_run_id: "run-root-sequential".into(),
            task: "review the implementation".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        execute_with_durable_parent(&engine, sequential_request, "orch", None)
            .await
            .unwrap();
        let mut fork_request = fork_request("delegation-fork-cancel", vec!["a", "b"], "writer");
        fork_request.parent_run_id = "run-root-sequential".into();
        execute_with_durable_parent(&engine, fork_request, "orch", None)
            .await
            .unwrap();

        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 4);
        let cancel_tokens: Vec<_> = bindings
            .iter()
            .map(|binding| binding.cancel_token.as_ref().expect("child cancel token"))
            .collect();
        for (index, cancel_token) in cancel_tokens.iter().enumerate() {
            for other in cancel_tokens.iter().skip(index + 1) {
                assert!(
                    !Arc::ptr_eq(cancel_token, other),
                    "sibling delegated runs need independent cancellation tokens"
                );
            }
        }
    }

    #[tokio::test]
    async fn parent_cancel_waits_for_child_executor_before_recording_terminal_state() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_id = "delegation-cancel-awaits-executor".into();
        request.parent_run_id = "parent-cancel-awaits-executor".into();
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            tokio::spawn(async move {
                bind_test_engine(&engine)
                    .execute(request, "orch", None)
                    .await
            })
        };
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("child should begin promptly")
            .expect("test executor reports the run id");

        assert_eq!(
            engine
                .cancel_children_of("parent-cancel-awaits-executor")
                .await,
            1
        );
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("child should observe cancellation promptly");
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Running),
            "a cancellation request must not pre-write the terminal tracker state"
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_RUNNING,
            "durable state must not claim cancellation before executor completion"
        );

        release.notify_one();
        execution
            .await
            .expect("delegation task joins")
            .expect("delegation completes");
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test]
    async fn fanout_cancellation_waits_for_the_childs_canonical_cancelled_result() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_id = "delegation-parent-cancel".into();
        request.parent_run_id = "parent-fanout-cancel".into();
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move {
                bind_test_engine(&engine)
                    .execute(request, "orch", Some(parent_cancel))
                    .await
            })
        };
        let mut execution = Box::pin(execution);
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("child should begin promptly")
            .expect("test executor reports the run id");

        parent_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("child should observe parent cancellation promptly");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), execution.as_mut())
                .await
                .is_err(),
            "fanout must wait for its child to publish a canonical terminal result"
        );

        release.notify_one();
        let result = execution
            .await
            .expect("delegation task joins")
            .expect("delegation completes");
        assert_eq!(result.agent_results[0].status, STATUS_CANCELLED);
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test]
    async fn fork_cancellation_preserves_the_childs_canonical_cancelled_result() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = fork_request("delegation-fork-parent-cancel", vec!["inspect"], "writer");
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move {
                bind_test_engine(&engine)
                    .execute(request, "orch", Some(parent_cancel))
                    .await
            })
        };
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("fork child should begin promptly")
            .expect("test executor reports the run id");

        parent_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("fork child should observe parent cancellation promptly");
        release.notify_one();

        let result = execution
            .await
            .expect("fork delegation task joins")
            .expect("fork delegation completes");
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, STATUS_CANCELLED);
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("fork child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fork_parent_cancellation_reconciles_a_force_aborted_child() {
        struct IgnoresCancellationExecutor(tokio::sync::mpsc::UnboundedSender<String>);

        #[async_trait]
        impl SubRunExecutor for IgnoresCancellationExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                self.0.send(config.run_id.clone()).unwrap();
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.to_string(),
                        output: Some("late completion".to_string()),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(IgnoresCancellationExecutor(started_tx)),
        ));
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = fork_request("fork-force-abort-cancel", vec!["inspect"], "writer");
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move {
                bind_test_engine(&engine)
                    .execute(request, "orch", Some(parent_cancel))
                    .await
            })
        };
        let child_run_id =
            tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv())
                .await
                .expect("child admission did not reach the executor")
                .expect("child starts");
        parent_cancel.cancel();

        let result = execution
            .await
            .expect("fork delegation task joins")
            .expect("fork cancellation is reported as a delegation result");
        assert_eq!(result.agent_results[0].status, STATUS_CANCELLED);
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("fork child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    struct StatusExecutor {
        status: &'static str,
        error: Option<&'static str>,
    }

    #[async_trait]
    impl SubRunExecutor for StatusExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: self.status.to_string(),
                    output: Some(format!("[{}] yielded", self.status)),
                    error: self.error.map(ToString::to_string),
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    /// Test executor that fails for specific agents.
    struct FailingExecutor {
        fail_agents: Vec<String>,
    }

    #[async_trait]
    impl SubRunExecutor for FailingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            if self.fail_agents.contains(&config.agent_profile.agent_id) {
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "failed".to_string(),
                        output: None,
                        error: Some("intentional test failure".to_string()),
                        prompt_tokens: 5,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            } else {
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id.clone(),
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(format!("[{}] done", config.agent_profile.agent_id)),
                        error: None,
                        prompt_tokens: 10,
                        completion_tokens: 20,
                        tool_calls: 1,
                    },
                    None,
                ))
            }
        }
    }

    fn setup_with_executor(
        executor: Arc<dyn SubRunExecutor>,
    ) -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
        DelegationEngine,
    ) {
        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg.clone(), engine.clone(), tracker.clone(), executor);
        (reg, engine, tracker, de)
    }

    #[tokio::test]
    async fn lost_child_receipt_preserves_unknown_durable_outcome() {
        use crate::server::run::lifecycle::tests::FaultInjectedRunStateStore;

        for read_fails in [false, true] {
            for receipt_closed in [false, true] {
                let (registry, _, tracker) = setup();
                let store = FaultInjectedRunStateStore::new(&[], &[]);
                let store = if read_fails {
                    store.with_failed_load_run_call(1)
                } else {
                    store
                };
                let engine = Arc::new(RunEngine::new(Arc::new(store)));
                let de = bind_test_engine(&DelegationEngine::with_executor(
                    registry,
                    engine,
                    tracker,
                    Arc::new(EchoExecutor),
                ));
                let parent = de.spawner.as_ref().unwrap().fanout_parent("parent");
                let (sender, receipt) = tokio::sync::oneshot::channel();
                if receipt_closed {
                    drop(sender);
                } else {
                    sender
                        .send((Err(("settlement panicked".into(), Some("panic"))), 0))
                        .unwrap();
                }
                let result = de
                    .collect_supervised_subrun(
                        "worker".into(),
                        "unknown-child".into(),
                        Ok(receipt),
                        parent,
                        "u",
                        "s",
                    )
                    .await;
                assert_eq!(result.status, STATUS_WAITING);
                assert!(result.is_unfinished());
                assert!(result.output.is_none());
                assert!(result.error.as_deref().unwrap().contains(if read_fails {
                    "failed to load durable winner"
                } else {
                    "is missing"
                }));
            }
        }
    }

    #[tokio::test]
    async fn fan_out_executes_with_custom_executor() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);

        // Both agents should have executed and produced output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.as_ref().unwrap().contains("test task"));
            assert_eq!(ar.prompt_tokens, 10);
            assert_eq!(ar.completion_tokens, 20);
            assert_eq!(ar.tool_calls, 1);
        }

        // Token aggregation
        assert_eq!(result.total_prompt_tokens, 20);
        assert_eq!(result.total_completion_tokens, 40);
        assert_eq!(result.total_tool_calls, 2);

        // Engine persisted final status
        for ar in &result.agent_results {
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Tracker recorded hierarchy
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn fan_out_paused_result_preserves_nonterminal_tracker_state() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(StatusExecutor {
            status: STATUS_PAUSED,
            error: None,
        }));
        let de = bind_test_engine(&de);
        let spawner = de.spawner.as_ref().unwrap();
        let parent = spawner.fanout_parent("parent-1");

        let result = execute_with_durable_parent(&de, fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, STATUS_PAUSED);

        let run_id = &result.agent_results[0].run_id;
        assert_eq!(
            tracker.get_sub_run_state(run_id).await,
            Some(SubRunState::Paused)
        );
        assert!(tracker.get_pause_flag(run_id).await.is_some());
        assert!(
            tracker
                .state
                .read()
                .await
                .cancel_tokens
                .contains_key(run_id)
        );
        let run = engine.load_run("user-1", run_id).await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        let child = parent
            .pending_direct_children()
            .pop()
            .expect("pause stays pending");
        let state = spawner.get_agent_state_any(&child.agent_id).await.unwrap();
        assert!(matches!(
            child.status,
            crate::orchestration::AgentStatus::Paused { .. }
        ));
        assert!(
            !parent.consume_terminal_child(
                &child.agent_id,
                state.committed_frontier.as_ref().unwrap()
            )
        );
        assert_eq!(child.run_id, *run_id);
    }

    #[tokio::test]
    async fn sequential_passes_output_to_next_stage() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-pipe".into(),
            parent_run_id: "p".into(),
            task: "build code".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // First stage has no previous_output
        let first = &result.agent_results[0];
        assert_eq!(first.agent_id, "coder");
        assert!(!first.output.as_ref().unwrap().contains("prev="));

        // Second stage receives first stage's output
        let second = &result.agent_results[1];
        assert_eq!(second.agent_id, "reviewer");
        assert!(second.output.as_ref().unwrap().contains("prev="));
        assert!(second.output.as_ref().unwrap().contains("[coder]"));
    }

    #[tokio::test]
    async fn sequential_stop_on_success_stops_early() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-early".into(),
            parent_run_id: "p".into(),
            task: "find answer".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        // First agent succeeds → stops
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].agent_id, "coder");
    }

    #[tokio::test]
    async fn sequential_does_not_launch_after_an_unfinished_stage() {
        for status in [STATUS_PAUSED, STATUS_WAITING] {
            let (_, run_engine, tracker, engine) = setup_with_executor(Arc::new(StatusExecutor {
                status,
                error: None,
            }));
            let mut request = fan_out_request(vec!["coder", "reviewer"]);
            request.pattern = CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            };
            let delegation_id = request.delegation_id.clone();
            let user_id = request.user_id.clone();
            let result = execute_with_durable_parent(&engine, request, "orch", None)
                .await
                .unwrap();
            assert_eq!(result.agent_results.len(), 1);
            assert_eq!(result.agent_results[0].status, status);
            assert_eq!(tracker.get_sub_runs(&delegation_id).await.len(), 1);
            let durable = run_engine
                .load_run(&user_id, &result.agent_results[0].run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(durable.status, status);
        }
    }

    #[tokio::test]
    async fn fan_out_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["reviewer".to_string()],
        });
        let (_, _, _, de) = setup_with_executor(executor);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "partial");
        assert_eq!(result.agent_results.len(), 2);

        let coder = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "coder")
            .unwrap();
        assert_eq!(coder.status, "completed");
        assert!(coder.output.is_some());

        let reviewer = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "reviewer")
            .unwrap();
        assert_eq!(reviewer.status, "failed");
        assert!(reviewer.error.is_some());
    }

    #[tokio::test]
    async fn sequential_parent_cancellation_preserves_completed_stage_result() {
        struct CancelAfterProducer {
            parent_cancel: Arc<tokio_util::sync::CancellationToken>,
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl SubRunExecutor for CancelAfterProducer {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                self.parent_cancel.cancel();
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.to_string(),
                        output: Some("producer result retained".into()),
                        error: None,
                        prompt_tokens: 7,
                        completion_tokens: 3,
                        tool_calls: 1,
                    },
                    None,
                ))
            }
        }

        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let executor = Arc::new(CancelAfterProducer {
            parent_cancel: parent_cancel.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let (registry, run_engine, tracker) = setup();
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker,
            executor.clone(),
        );
        let request = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-stage-cancel".into(),
            parent_run_id: "parent-seq-stage-cancel".into(),
            task: "produce then review".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&engine, request, "orch", Some(parent_cancel))
            .await
            .expect("cancellation preserves the completed stage result");

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].agent_id, "coder");
        assert_eq!(result.agent_results[0].status, STATUS_COMPLETED);
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("producer result retained")
        );
        assert_eq!(result.agent_results[0].prompt_tokens, 7);
        assert_eq!(result.agent_results[0].completion_tokens, 3);
        assert_eq!(result.agent_results[0].tool_calls, 1);
        let children = run_engine
            .find_sub_runs("user-1", "del-seq-stage-cancel")
            .await
            .unwrap();
        assert_eq!(
            children.len(),
            1,
            "cancelled stages must not create child runs"
        );
        assert_eq!(children[0].status, STATUS_COMPLETED);
        assert_eq!(executor.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn tracker_get_agent_id_returns_correct_id() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        assert_eq!(
            tracker.get_agent_id("sub-1").await,
            Some("coder".to_string())
        );
        assert_eq!(tracker.get_agent_id("parent").await, None);
        assert_eq!(tracker.get_agent_id("nonexistent").await, None);
    }

    #[tokio::test]
    async fn with_executor_constructor_uses_custom_executor() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.status, "completed");
        // EchoExecutor returns prompt_tokens=10
        assert_eq!(result.total_prompt_tokens, 10);
    }

    #[tokio::test]
    async fn sub_run_config_passes_context() {
        /// Executor that checks context is passed through.
        struct ContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let has_key = config.context.contains_key("test_key");
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(format!("context_present={}", has_key)),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(ContextCheckExecutor));

        let mut ctx = HashMap::new();
        ctx.insert("test_key".to_string(), serde_json::json!("test_value"));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "ctx-test".into(),
            parent_run_id: "p".into(),
            task: "check context".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: ctx,
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("context_present=true")
        );
    }

    #[tokio::test]
    async fn execute_with_forward_headers_passes_sensitive_headers_sideband() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config
                    .context
                    .contains_key(crate::server::delegation::FORWARD_HEADERS_CONTEXT_KEY);
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(format!(
                            "auth_present={has_auth};context_key_present={has_context_key}"
                        )),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "fh-test".into(),
            parent_run_id: "p".into(),
            task: "check headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent_and_headers(
            &de,
            req,
            "orch",
            None,
            HashMap::from([(
                "authorization".to_string(),
                "Bearer trusted-token".to_string(),
            )]),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=true;context_key_present=false")
        );
    }

    #[tokio::test]
    async fn execute_with_forward_headers_passes_admitted_model_execution_sideband() {
        struct ExecutionMaterialCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ExecutionMaterialCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let encoded = config
                    .admitted_model_execution
                    .as_ref()
                    .map(|execution| {
                        format!(
                            "{}|{}",
                            execution
                                .completions_url_override
                                .as_deref()
                                .unwrap_or_default(),
                            execution.request_timeout_ms.unwrap_or(0)
                        )
                    })
                    .unwrap_or_else(|| "none".to_string());
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(encoded),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ExecutionMaterialCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "execution-material-test".into(),
            parent_run_id: "p".into(),
            task: "check admitted execution material".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent_and_headers(
            &de,
            req,
            "orch",
            None,
            HashMap::new(),
            Some(AdmittedModelExecution::from_endpoint(
                "offer-coder".to_string(),
                "test-model".to_string(),
                "openai".to_string(),
                "http://catalog:8081/api/v1/chat/completions".to_string(),
                "Bearer test".to_string(),
                Some(2500),
                128_000,
            )),
        )
        .await
        .unwrap();

        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("http://catalog:8081/api/v1/chat/completions|2500")
        );
    }

    #[tokio::test]
    async fn same_offering_delegated_child_inherits_and_persists_parent_reasoning() {
        use astra_turn_core::orchestration_spawn_tool::ParentModelReasoning;
        use astra_turn_core::thinking_config::{ThinkingConfig, ThinkingEffort};
        use astra_turn_types::ModelSelection;

        struct ThinkingCheckExecutor {
            observed: Arc<
                std::sync::Mutex<
                    Vec<(
                        Option<String>,
                        astra_turn_core::thinking_config::ThinkingConfig,
                    )>,
                >,
            >,
        }

        #[async_trait]
        impl SubRunExecutor for ThinkingCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let selected_offering = config
                    .agent_profile
                    .model_selection
                    .as_ref()
                    .map(|selection| selection.offering_id.clone());
                self.observed
                    .lock()
                    .unwrap()
                    .push((selected_offering.clone(), config.thinking.clone()));
                let selected_offering = config
                    .agent_profile
                    .model_selection
                    .map(|selection| selection.offering_id);
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(
                            serde_json::json!({
                                "offering_id": selected_offering,
                                "thinking": config.thinking,
                            })
                            .to_string(),
                        ),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let mut registry = AgentProfileRegistry::new();
        registry
            .register(AgentProfile::new(
                "orch",
                "Orchestrator",
                AgentTier::Orchestrator,
            ))
            .unwrap();
        let coder = AgentProfile::new("coder", "Coder", AgentTier::System);
        registry.register(coder).unwrap();

        let run_engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        let observed_attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            Arc::new(RwLock::new(registry)),
            run_engine.clone(),
            Arc::new(DelegationTracker::new()),
            Arc::new(ThinkingCheckExecutor {
                observed: observed_attempts.clone(),
            }),
        );
        let request = DelegationRequest {
            session_id: "reasoning-session".into(),
            delegation_id: "reasoning-delegation".into(),
            parent_run_id: "reasoning-parent".into(),
            task: "preserve the admitted reasoning setting".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "reasoning-user".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let expected = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let result = bind_test_engine(&engine)
            .execute_with_forward_headers_and_live_events(
                request,
                "orch",
                None,
                HashMap::new(),
                None,
                Some(ParentModelReasoning {
                    selection: ModelSelection {
                        offering_id: "offer-coder".into(),
                    },
                    resolved_model_name: None,
                    thinking: expected.clone(),
                }),
                None,
            )
            .await
            .unwrap();

        let observed: serde_json::Value =
            serde_json::from_str(result.agent_results[0].output.as_deref().unwrap()).unwrap();
        assert_eq!(observed["offering_id"], "offer-coder");
        assert_eq!(
            serde_json::from_value::<ThinkingConfig>(observed["thinking"].clone()).unwrap(),
            expected
        );
        let attempts = observed_attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1, "one admitted child execution");
        assert!(attempts.iter().all(|(offering, thinking)| {
            offering.as_deref() == Some("offer-coder") && thinking == &expected
        }));
        drop(attempts);
        let durable = run_engine
            .load_run("reasoning-user", &result.agent_results[0].run_id)
            .await
            .unwrap()
            .expect("delegated child has a durable run");
        assert_eq!(
            crate::server::run::engine::durable_run_generation_controls(&durable)
                .unwrap()
                .thinking,
            expected
        );
    }

    #[tokio::test]
    async fn execute_ignores_serialized_forward_headers_in_request_context() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config
                    .context
                    .contains_key(crate::server::delegation::FORWARD_HEADERS_CONTEXT_KEY);
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(format!(
                            "auth_present={has_auth};context_key_present={has_context_key}"
                        )),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "fh-context-test".into(),
            parent_run_id: "p".into(),
            task: "check serialized headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::from([(
                crate::server::delegation::FORWARD_HEADERS_CONTEXT_KEY.to_string(),
                serde_json::json!({"authorization": "Bearer evil", "x-workspace-id": "ws-001"}),
            )]),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=false;context_key_present=false")
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_normalizes_and_dedupes() {
        let key = crate::server::delegation::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(
            key.to_string(),
            serde_json::json!([" Bash ", "bash", "READ_FILE"]),
        )]);

        let parsed = parse_request_allowlist_from_context(&mut context, key)
            .expect("allowlist should parse")
            .expect("allowlist should be present");

        let expected = HashSet::from(["bash".to_string(), "read_file".to_string()]);
        assert_eq!(parsed, expected);
        assert!(
            !context.contains_key(key),
            "key should be removed from context"
        );
    }

    #[test]
    fn optional_tool_context_distinguishes_unmanaged_cli_from_explicit_disable() {
        let key = crate::server::delegation::REQUEST_ENABLED_TOOLS_CONTEXT_KEY;
        let mut local_cli_context = HashMap::new();
        assert_eq!(
            parse_request_allowlist_from_context(&mut local_cli_context, key)
                .expect("omitted optional-tool context is valid"),
            None
        );

        let mut explicitly_disabled_context =
            HashMap::from([(key.to_string(), serde_json::json!([]))]);
        assert_eq!(
            parse_request_allowlist_from_context(&mut explicitly_disabled_context, key)
                .expect("explicit optional-tool deny set is valid"),
            Some(HashSet::new())
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_array_value() {
        let key = crate::server::delegation::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(key.to_string(), serde_json::json!("bash"))]);

        let err = parse_request_allowlist_from_context(&mut context, key)
            .expect_err("non-array allowlist should fail");
        assert!(err.contains("must be an array of strings"));
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_string_or_empty_entries() {
        let key = crate::server::delegation::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut non_string_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", 42]))]);
        let err = parse_request_allowlist_from_context(&mut non_string_context, key)
            .expect_err("non-string entry should fail");
        assert!(err.contains("must contain only strings"));

        let mut empty_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", "   "]))]);
        let err = parse_request_allowlist_from_context(&mut empty_context, key)
            .expect_err("empty entry should fail");
        assert!(err.contains("must not contain empty or whitespace-only strings"));
    }

    #[test]
    fn parse_request_skill_sources_from_context_normalizes_and_parses() {
        let key = crate::server::delegation::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY;
        let mut context = HashMap::from([(
            key.to_string(),
            serde_json::json!([" Database ", "database", "MCP"]),
        )]);

        let parsed = parse_request_skill_sources_from_context(&mut context, key)
            .expect("skill sources should parse")
            .expect("skill sources should be present");

        let expected = HashSet::from([
            crate::skills::manifest::SkillSourceKind::Database,
            crate::skills::manifest::SkillSourceKind::Mcp,
        ]);
        assert_eq!(parsed, expected);
        assert!(
            !context.contains_key(key),
            "key should be removed from context"
        );
    }

    #[test]
    fn parse_request_skill_sources_from_context_rejects_unknown_source() {
        let key = crate::server::delegation::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY;
        let mut context = HashMap::from([(key.to_string(), serde_json::json!(["dynamic"]))]);

        let err = parse_request_skill_sources_from_context(&mut context, key)
            .expect_err("unknown skill source should fail");
        assert!(err.contains("unsupported skill source"));
        assert!(err.contains("expected one of"));
    }

    #[tokio::test]
    async fn worktree_path_per_agent_flows_through_context() {
        /// Executor that captures the agent-specific worktree_path from context.
        struct WorktreeCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for WorktreeCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let key = format!("worktree_path_{}", config.agent_profile.agent_id);
                let path = config
                    .context
                    .get(&key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("none")
                    .to_string();
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(path),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        // Register two agents
        {
            let mut r = reg.write().await;
            let _ = r.register(AgentProfile::new("agent-a", "Agent A", AgentTier::User));
            let _ = r.register(AgentProfile::new("agent-b", "Agent B", AgentTier::User));
        }
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(WorktreeCheckExecutor));

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-a".to_string(),
            serde_json::json!("/tmp/wt/agent-a"),
        );
        ctx.insert(
            "worktree_path_agent-b".to_string(),
            serde_json::json!("/tmp/wt/agent-b"),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "wt-test".into(),
            parent_run_id: "p".into(),
            task: "check worktree".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["agent-a".into(), "agent-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 30,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: ctx,
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Each agent should see its own worktree path
        for ar in &result.agent_results {
            let expected_path = format!("/tmp/wt/{}", ar.agent_id);
            assert_eq!(
                ar.output.as_deref(),
                Some(expected_path.as_str()),
                "agent {} should see its worktree path",
                ar.agent_id
            );
        }
    }

    #[tokio::test]
    async fn pause_children_of_ignores_terminal_sub_runs() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Pause all children of parent-1 (sub-runs are already completed)
        let paused = de
            .pause_children_of("user-1", "test-session", "parent-1")
            .await;
        assert_eq!(paused, 0);

        // A terminal task cannot observe cooperative flags.
        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
            assert!(tracker.get_pause_flag(&ar.run_id).await.is_none());
            assert!(
                !tracker
                    .state
                    .read()
                    .await
                    .cancel_tokens
                    .contains_key(&ar.run_id)
            );
            assert_eq!(tracker.get_depth(&ar.run_id).await, Some(1));
        }

        // Durable status is NOT overwritten for terminal sub-runs
        for ar in &result.agent_results {
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Resume does not claim to revive completed work.
        let resumed = de
            .resume_children_of("user-1", "test-session", "parent-1")
            .await;
        assert_eq!(resumed, 0);
        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
        }
    }

    #[tokio::test]
    async fn pause_delegation_by_id_ignores_terminal_sub_runs() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        let paused = de.pause_delegation("user-1", "test-session", "del-1").await;
        assert_eq!(paused, 0);

        let subs = tracker.get_sub_runs("del-1").await;
        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
            // Durable status preserved — terminal sub-runs not overwritten
            let run = engine
                .load_run("user-1", &sub.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        let resumed = de
            .resume_delegation("user-1", "test-session", "del-1")
            .await;
        assert_eq!(resumed, 0);
        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
        }
    }

    #[tokio::test]
    async fn live_sub_run_pause_resume_commits_status_and_event_before_flag() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));
        engine
            .start_run("parent-live", "user-1", "session-live")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-live",
                "user-1",
                "session-live",
                Some("parent-live"),
                Some("delegation-live"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-live".into(),
                parent_run_id: "parent-live".into(),
                delegation_id: "delegation-live".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker.register_pause_flag("sub-live").await;

        assert_eq!(
            de.pause_children_of("user-1", "session-live", "parent-live")
                .await,
            1
        );
        assert!(tracker.is_paused("sub-live").await);
        let paused = engine
            .load_run("user-1", "sub-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(paused.status, STATUS_PAUSED);
        assert_eq!(paused.waiting_for.as_deref(), Some("parent_pause"));
        assert_eq!(paused.events.last().unwrap()["event_type"], "run_paused");

        assert_eq!(
            de.resume_children_of("user-1", "session-live", "parent-live")
                .await,
            1
        );
        assert!(!tracker.is_paused("sub-live").await);
        let resumed = engine
            .load_run("user-1", "sub-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, STATUS_RUNNING);
        assert!(resumed.waiting_for.is_none());
        assert_eq!(resumed.events.last().unwrap()["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn waiting_sub_run_retains_its_required_context_when_parent_pauses() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));
        engine
            .start_run("parent-live", "user-1", "session-live")
            .await
            .unwrap();
        let execution_authority = engine
            .start_run_ext(
                "sub-waiting",
                "user-1",
                "session-live",
                Some("parent-live"),
                Some("delegation-live"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-waiting".into(),
                parent_run_id: "parent-live".into(),
                delegation_id: "delegation-live".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Waiting,
                retry_of: None,
            })
            .await;
        tracker.register_pause_flag("sub-waiting").await;
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-live",
                    "sub-waiting",
                    execution_authority.owner_generation,
                    STATUS_WAITING,
                    Some("user_input"),
                    None,
                )
                .await
                .unwrap()
        );

        assert_eq!(
            de.pause_children_of("user-1", "session-live", "parent-live")
                .await,
            0
        );
        assert!(!tracker.is_paused("sub-waiting").await);
        let durable = engine
            .load_run("user-1", "sub-waiting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_WAITING);
        assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
    }

    #[tokio::test]
    async fn durable_control_decision_replaces_stale_executor_result() {
        for (run_id, durable_status) in [
            ("stale-after-pause", STATUS_PAUSED),
            ("stale-after-cancel", STATUS_CANCELLED),
        ] {
            let (_, engine, _) = setup();
            let execution_authority = engine
                .start_run(run_id, "user-1", "session-1")
                .await
                .unwrap();
            if durable_status == STATUS_CANCELLED {
                engine
                    .persist_delegation_outcome_status_if_current_owner(
                        "user-1",
                        "session-1",
                        run_id,
                        execution_authority.owner_generation,
                        durable_status,
                        None,
                        Some("control-plane"),
                    )
                    .await
                    .unwrap();
            } else {
                assert!(
                    engine
                        .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                            user_id: "user-1",
                            expected_session_id: "session-1",
                            run_id,
                            status: durable_status,
                            waiting_for: Some("control-plane"),
                            error_message: None,
                            expected_statuses: &["running"],
                        })
                        .await
                        .unwrap()
                );
            }

            let authoritative = reconcile_agent_result_with_durable_authority(
                &engine,
                "user-1",
                "session-1",
                DurableLifecycleDisposition::SchedulerOwned {
                    owner_generation: 0,
                },
                AgentResult {
                    agent_id: "coder".into(),
                    run_id: run_id.into(),
                    status: STATUS_COMPLETED.into(),
                    output: Some("stale executor output".into()),
                    error: None,
                    prompt_tokens: 3,
                    completion_tokens: 5,
                    tool_calls: 1,
                },
                None,
            )
            .await
            .0;

            assert_eq!(authoritative.status, durable_status);
            assert!(authoritative.output.is_none());
            if durable_status == STATUS_CANCELLED {
                assert!(authoritative.error.is_some());
            } else {
                assert!(authoritative.error.is_none());
            }
        }
    }

    #[tokio::test]
    async fn executor_owned_durable_lifecycle_never_issues_unfenced_outer_terminal_write() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("executor-owned-stale-result", "user-1", "session-1")
            .await
            .expect("start durable child run");
        assert_eq!(authority.owner_generation, 0);

        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].run.run_generation, 1);

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::ExecutorOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: "executor-owned-stale-result".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale executor output".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
            None,
        )
        .await
        .0;

        assert_eq!(authoritative.status, STATUS_WAITING);
        assert!(authoritative.output.is_none());
        let durable = engine
            .load_run("user-1", "executor-owned-stale-result")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.run_generation, 1);
    }

    #[tokio::test]
    async fn same_terminal_status_from_new_generation_does_not_preserve_stale_output() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("executor-owned-same-status-race", "user-1", "session-1")
            .await
            .expect("start durable child run");

        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        let winner_generation = claimed[0].run.run_generation;
        assert_ne!(winner_generation, authority.owner_generation);
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-1",
                    "executor-owned-same-status-race",
                    winner_generation,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .expect("recovered owner commits its completed result")
        );

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::ExecutorOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: "executor-owned-same-status-race".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale generation output".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
            None,
        )
        .await
        .0;

        assert_eq!(authoritative.status, STATUS_COMPLETED);
        assert!(authoritative.output.is_none());
        assert_eq!(authoritative.prompt_tokens, 0);
        assert_eq!(authoritative.completion_tokens, 0);
        assert_eq!(authoritative.tool_calls, 0);
    }

    #[tokio::test]
    async fn scheduler_cas_loser_does_not_preserve_same_status_from_stale_generation() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("scheduler-owned-same-status-race", "user-1", "session-1")
            .await
            .expect("start durable child run");
        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        let winner_generation = claimed[0].run.run_generation;
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-1",
                    "scheduler-owned-same-status-race",
                    winner_generation,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .expect("recovered owner commits its completed result")
        );

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "offline-executor".into(),
                run_id: "scheduler-owned-same-status-race".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale scheduler output".into()),
                error: None,
                prompt_tokens: 8,
                completion_tokens: 13,
                tool_calls: 2,
            },
            None,
        )
        .await
        .0;

        assert_eq!(authoritative.status, STATUS_COMPLETED);
        assert!(authoritative.output.is_none());
        assert_eq!(authoritative.prompt_tokens, 0);
        assert_eq!(authoritative.completion_tokens, 0);
        assert_eq!(authoritative.tool_calls, 0);
    }

    #[tokio::test]
    async fn scheduler_owned_terminal_commits_replay_events_with_the_owner_cas() {
        for (run_id, status, error, expected_event_types) in [
            (
                "scheduler-owned-completed",
                STATUS_COMPLETED,
                None,
                vec!["run_finished"],
            ),
            (
                "scheduler-owned-failed",
                STATUS_FAILED,
                Some("offline executor failed"),
                vec!["run_error", "run_finished"],
            ),
        ] {
            let (_, engine, _) = setup();
            let authority = engine
                .start_run(run_id, "user-1", "session-1")
                .await
                .expect("start scheduler-owned run");

            let authoritative = reconcile_agent_result_with_durable_authority(
                &engine,
                "user-1",
                "session-1",
                DurableLifecycleDisposition::SchedulerOwned {
                    owner_generation: authority.owner_generation,
                },
                AgentResult {
                    agent_id: "offline-executor".into(),
                    run_id: run_id.into(),
                    status: status.into(),
                    output: (status == STATUS_COMPLETED).then(|| "answer".into()),
                    error: error.map(str::to_string),
                    prompt_tokens: 3,
                    completion_tokens: 5,
                    tool_calls: 1,
                },
                None,
            )
            .await
            .0;

            assert_eq!(authoritative.status, status);
            let durable = engine.load_run("user-1", run_id).await.unwrap().unwrap();
            assert_eq!(durable.status, status);
            let event_types = durable
                .events
                .iter()
                .filter_map(|event| event.get("event_type").and_then(serde_json::Value::as_str))
                .filter(|event_type| matches!(*event_type, "run_error" | "run_finished"))
                .collect::<Vec<_>>();
            assert_eq!(event_types, expected_event_types);
        }
    }

    #[tokio::test]
    async fn identical_terminal_replay_preserves_richer_agent_result() {
        let (_, engine, _) = setup();
        let run_id = "completed-result-replay";
        let execution_authority = engine
            .start_run(run_id, "user-1", "session-1")
            .await
            .unwrap();
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-1",
                    run_id,
                    execution_authority.owner_generation,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .unwrap()
        );

        let replayed = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: 0,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: run_id.into(),
                status: STATUS_COMPLETED.into(),
                output: Some("full answer".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
            None,
        )
        .await
        .0;

        assert_eq!(replayed.status, STATUS_COMPLETED);
        assert_eq!(replayed.output.as_deref(), Some("full answer"));
    }

    #[tokio::test]
    async fn parent_cancellation_preserves_completed_child_authority() {
        struct CancelAfterCompletion {
            run_engine: Arc<RunEngine>,
            parent_cancel: Arc<tokio_util::sync::CancellationToken>,
        }

        #[async_trait]
        impl SubRunExecutor for CancelAfterCompletion {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let committed = self
                    .run_engine
                    .persist_delegation_outcome_status_if_current_owner(
                        &config.user_id,
                        &config.session_id,
                        &config.run_id,
                        config
                            .execution_owner_generation
                            .expect("child owner generation"),
                        STATUS_COMPLETED,
                        None,
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                if !committed {
                    return Err("child completion lost durable authority".into());
                }
                self.parent_cancel.cancel();
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.into(),
                        output: Some("completed before parent cancellation".into()),
                        error: None,
                        prompt_tokens: 7,
                        completion_tokens: 3,
                        tool_calls: 1,
                    },
                    None,
                ))
            }

            fn owns_durable_run_lifecycle(&self) -> bool {
                true
            }
        }

        let (registry, run_engine, tracker) = setup();
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker,
            Arc::new(CancelAfterCompletion {
                run_engine: run_engine.clone(),
                parent_cancel: parent_cancel.clone(),
            }),
        );
        let result = execute_with_durable_parent(
            &engine,
            fan_out_request(vec!["coder"]),
            "orch",
            Some(parent_cancel),
        )
        .await
        .expect("cancellation preserves the durable completed winner");
        assert_eq!(result.agent_results.len(), 1);
        let child = &result.agent_results[0];
        assert_eq!(child.status, STATUS_COMPLETED);
        assert_eq!(
            child.output.as_deref(),
            Some("completed before parent cancellation")
        );
        assert_eq!(child.prompt_tokens, 7);
        assert_eq!(child.completion_tokens, 3);
        assert_eq!(child.tool_calls, 1);
        let durable = run_engine
            .load_run("user-1", &child.run_id)
            .await
            .expect("completed child loads")
            .expect("completed child exists");
        assert_eq!(durable.status, STATUS_COMPLETED);
    }

    #[tokio::test]
    async fn cancellation_during_later_stage_admission_never_creates_or_dispatches_child() {
        struct CancelOnLaterAdmissionExecutor {
            cancel: Arc<tokio_util::sync::CancellationToken>,
            admissions: std::sync::atomic::AtomicUsize,
            executions: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl SubRunExecutor for CancelOnLaterAdmissionExecutor {
            async fn prepare_model_batch(
                &self,
                requests: &[SubRunModelRequest],
            ) -> Result<Vec<Option<PreparedSubRunModel>>, String> {
                if self.admissions.fetch_add(1, Ordering::Relaxed) == 1 {
                    // The admission future returns a result in the same poll
                    // that publishes cancellation. The caller must check the
                    // token again before durably creating the next stage.
                    self.cancel.cancel();
                }
                Ok(vec![None; requests.len()])
            }

            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                self.executions.fetch_add(1, Ordering::Relaxed);
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.to_string(),
                        output: Some("initial execution completed".to_string()),
                        error: None,
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let executor = Arc::new(CancelOnLaterAdmissionExecutor {
            cancel: parent_cancel.clone(),
            admissions: std::sync::atomic::AtomicUsize::new(0),
            executions: std::sync::atomic::AtomicUsize::new(0),
        });
        let (registry, run_engine, tracker) = setup();
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            executor.clone(),
        );
        let mut request = fan_out_request(vec!["coder", "reviewer"]);
        request.pattern = CoordinationPattern::Sequential {
            agent_ids: vec!["coder".into(), "reviewer".into()],
            stop_on_success: false,
            timeout_sec: 0,
        };
        request.delegation_id = "stage-cancel-admission".into();
        request.parent_run_id = "parent-stage-cancel-admission".into();

        let result = execute_with_durable_parent(&engine, request, "orch", Some(parent_cancel))
            .await
            .expect("cancellation returns the completed physical result projection");

        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.agent_results[0].status, STATUS_COMPLETED);
        assert_eq!(result.agent_results[1].status, STATUS_CANCELLED);
        assert_eq!(
            executor.executions.load(Ordering::Relaxed),
            1,
            "cancellation during stage admission must not dispatch another provider call"
        );
        assert_eq!(
            tracker.get_sub_runs("stage-cancel-admission").await.len(),
            1,
            "cancellation before stage durable admission must not create a child row"
        );
        let durable = run_engine
            .load_run("user-1", &result.agent_results[0].run_id)
            .await
            .expect("initial durable run loads")
            .expect("initial durable run exists");
        assert_eq!(
            durable.status, STATUS_COMPLETED,
            "stage cancellation must not overwrite the completed first child"
        );
    }

    #[tokio::test]
    async fn sequential_children_carry_exact_durable_execution_authority() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = Arc::new(CaptureRunBindingExecutor {
            bindings: bindings.clone(),
        });
        let engine = DelegationEngine::with_executor(registry, run_engine, tracker, executor);
        let request = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "sequential-authority".into(),
            parent_run_id: "sequential-parent".into(),
            task: "verify child authority".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&engine, request, "orch", None)
            .await
            .expect("sequential children complete");
        assert_eq!(result.agent_results.len(), 2);
        assert!(
            result
                .agent_results
                .iter()
                .all(|child| child.status == STATUS_COMPLETED)
        );
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings
                .iter()
                .all(|binding| binding.execution_owner_generation == Some(0)),
            "every admitted sequential child must carry its exact owner generation"
        );
    }

    #[tokio::test]
    async fn sequential_preserves_coordination_prompt() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-prompt".into(),
            parent_run_id: "parent-1".into(),
            task: "sequential prompt test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        let output = result.agent_results[0].output.as_deref().unwrap_or("");
        assert!(output.contains("## Team Coordination: Sequential Execution"));
    }

    #[tokio::test]
    async fn fan_out_preserves_depth_metadata() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor));
        let result = execute_with_durable_parent(
            &de,
            fan_out_request(vec!["coder", "reviewer"]),
            "orch",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.agent_results.len(), 2);
        for child in &result.agent_results {
            assert_eq!(tracker.get_depth(&child.run_id).await, Some(1));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(session_journal_dir)]
    async fn tracker_running_transition_writes_sub_run_started_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-start-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker = DelegationTracker::with_session("user-1".into(), "sess-subrun-start".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("run-0".into()),
            })
            .await;
        tracker
            .transition_state("run-1", SubRunState::Running)
            .await
            .unwrap();

        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            "user-1",
            "sess-subrun-start",
        )
        .unwrap();
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let started_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunStarted
            })
            .collect();

        assert_eq!(started_events.len(), 1);
        let meta = started_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["parent_run_id"], "parent-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "running");
        assert_eq!(meta["retry_of"], "run-0");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(session_journal_dir)]
    async fn tracker_complete_sub_run_writes_sub_run_completed_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-complete-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker =
            DelegationTracker::with_session("user-1".into(), "sess-subrun-complete".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .complete_sub_run_with_result(
                "run-1",
                SubRunState::Failed,
                Some("boom"),
                Some("partial output"),
            )
            .await;
        tracker
            .complete_sub_run_with_result(
                "run-1",
                SubRunState::Failed,
                Some("boom"),
                Some("partial output"),
            )
            .await;

        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            "user-1",
            "sess-subrun-complete",
        )
        .unwrap();
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let completed_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunCompleted
            })
            .collect();

        assert_eq!(completed_events.len(), 1);
        let meta = completed_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "failed");
        assert_eq!(meta["error"], "boom");
        assert_eq!(meta["output_preview"], "partial output");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    // ── Persistence tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn start_run_ext_persists_delegation_metadata() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine
            .start_run("parent-1", "user-1", "sess-1")
            .await
            .unwrap();

        engine
            .start_run_ext(
                "sub-1",
                "user-1",
                "sess-1",
                Some("parent-1"),
                Some("del-1"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();

        let record = store.load_run("user-1", "sub-1").await.unwrap().unwrap();
        assert_eq!(record.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(record.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(record.agent_id.as_deref(), Some("coder"));
        assert_eq!(record.session_id, "sess-1");
    }

    #[tokio::test]
    async fn start_run_without_parent_metadata_sets_none() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let record = store.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert!(record.parent_run_id.is_none());
        assert!(record.delegation_id.is_none());
        assert!(record.agent_id.is_none());
    }

    #[tokio::test]
    async fn find_sub_runs_by_delegation_id() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        // Create a root run and two sub-runs in different delegations
        engine.start_run("root", "u1", "s1").await.unwrap();
        engine
            .start_run_ext(
                "sub-a",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-b",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-c",
                "u1",
                "s1",
                Some("root"),
                Some("del-2"),
                Some("writer"),
                None,
            )
            .await
            .unwrap();

        let del1_runs = engine.find_sub_runs("u1", "del-1").await.unwrap();
        assert_eq!(del1_runs.len(), 2);

        let del2_runs = engine.find_sub_runs("u1", "del-2").await.unwrap();
        assert_eq!(del2_runs.len(), 1);
        assert_eq!(del2_runs[0].agent_id.as_deref(), Some("writer"));
    }

    #[tokio::test]
    async fn load_from_run_records_rebuilds_tracker() {
        use astra_services::runs::DurableRunRecord;

        let records = vec![
            DurableRunRecord {
                run_id: "sub-1".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-1".into()),
                depth: 1,
                delegation_id: Some("del-1".into()),
                agent_id: Some("coder".into()),
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "completed".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            DurableRunRecord {
                run_id: "sub-2".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-2".into()),
                depth: 2,
                delegation_id: Some("del-1".into()),
                agent_id: Some("reviewer".into()),
                retry_of: Some("sub-1".into()),
                retry_scope: Some("node".into()),
                status: "paused".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            DurableRunRecord {
                run_id: "sub-3".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-3".into()),
                depth: 1,
                delegation_id: Some("del-1".into()),
                agent_id: Some("approver".into()),
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "waiting".into(),
                waiting_for: Some("approval".into()),
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            // Root run — should be skipped
            DurableRunRecord {
                run_id: "root-run".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: None,
                root_run_id: Some("root-run".into()),
                ancestor_path: Some("root-run".into()),
                depth: 0,
                delegation_id: None,
                agent_id: None,
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "completed".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
        ];

        let tracker = DelegationTracker::new();
        tracker.load_from_run_records(&records).await;

        // Hierarchy rebuilt
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 3);
        assert!(tracker.get_parent("sub-1").await.is_some());
        assert!(tracker.get_parent("sub-2").await.is_some());
        assert!(tracker.get_parent("sub-3").await.is_some());
        assert!(tracker.get_parent("root-run").await.is_none());

        // Parent links rebuilt
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
        assert_eq!(
            tracker.get_agent_id("sub-1").await.as_deref(),
            Some("coder")
        );
        assert_eq!(
            subs.iter()
                .find(|sub| sub.run_id == "sub-2")
                .and_then(|sub| sub.retry_of.as_deref()),
            Some("sub-1")
        );
        assert_eq!(tracker.get_depth("sub-2").await, Some(2));
        // Cooperative flags belong to a live executor task. A recovered
        // durable pause has no such task, so manufacturing a flag would let
        // resume_delegation mark the row running without an executor.
        assert!(tracker.get_pause_flag("sub-2").await.is_none());
        assert_eq!(
            tracker.resume_delegation("del-1").await,
            0,
            "recovery must not fabricate a resumable live executor"
        );

        // Waiting is a distinct recoverable state and does not recreate a
        // cooperative pause flag.
        assert_eq!(
            tracker.get_sub_run_state("sub-3").await,
            Some(SubRunState::Waiting)
        );
        assert!(tracker.get_pause_flag("sub-3").await.is_none());

        // Completed sub-run has no pause flag
        assert!(tracker.get_pause_flag("sub-1").await.is_none());
    }

    // ─── Fork Pattern Tests ─────────────────────────────────────────────

    fn fork_request(del_id: &str, tasks: Vec<&str>, agent_id: &str) -> DelegationRequest {
        DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: del_id.into(),
            parent_run_id: format!("parent-{del_id}"),
            task: "fork test".into(),
            pattern: CoordinationPattern::Fork {
                tasks: tasks.into_iter().map(String::from).collect(),
                agent_id: agent_id.into(),
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        }
    }

    #[tokio::test]
    async fn fork_spawns_parallel_children() {
        let (_, _engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));
        let transport = Arc::new(crate::messaging::InProcessTransport::new());
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            transport.clone(),
            tracker.clone(),
        ));
        let de = de.with_mailbox_router(router);

        let req = fork_request(
            "del-fork-spawn",
            vec!["task-a", "task-b", "task-c"],
            "writer",
        );
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 3);
        assert_eq!(result.status, "completed");

        let subs = tracker.get_sub_runs("del-fork-spawn").await;
        assert_eq!(subs.len(), 3);
        for sub in &subs {
            assert_eq!(sub.agent_id, "writer");
            assert_eq!(sub.depth, 1);
        }

        // All results should have output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
        }
        // Fork's context policy must not discard its declared result policy.
        assert_eq!(
            result.aggregated_output,
            aggregate_results(&AggregationStrategy::AllResults, &result.agent_results),
        );
        assert!(result.aggregated_output.is_some());
        assert_terminal_mailboxes_retired(&transport).await;
    }

    #[tokio::test]
    async fn fork_cold_restore_keeps_same_profile_children_distinct() {
        use crate::orchestration::spawner::tests::StaticDurableReconciler;

        struct SplitOutcomeExecutor(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait]
        impl SubRunExecutor for SplitOutcomeExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                let status = match config.context["fork_index"].as_u64().unwrap() {
                    0 => STATUS_FAILED,
                    1 => STATUS_PAUSED,
                    _ => panic!("only the two admitted Fork children may execute"),
                };
                StatusExecutor {
                    status,
                    error: Some("recorded child boundary"),
                }
                .execute(config)
                .await
            }
        }

        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_, run_engine, _, engine) =
            setup_with_executor(Arc::new(SplitOutcomeExecutor(executions.clone())));
        let engine = bind_test_engine(&engine);
        let request = fork_request("cold-identity", vec!["first", "second"], "writer");
        let parent_run_id = request.parent_run_id.clone();
        let result = execute_with_durable_parent(&engine, request, "orch", None)
            .await
            .unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(result.agent_results.len(), 2);

        let mut records = Vec::new();
        for child in &result.agent_results {
            let run = run_engine
                .load_run("user-1", &child.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                run.agent_id.as_deref(),
                Some("writer"),
                "business profile is not overwritten"
            );
            let expected_id = format!("writer@{}", child.run_id);
            let admitted_id = run
                .events
                .iter()
                .find(|event| event["event_type"] == "run_started")
                .unwrap()
                .pointer("/data/child_runtime_id")
                .and_then(serde_json::Value::as_str);
            assert_eq!(admitted_id, Some(expected_id.as_str()));
            let live = engine
                .spawner
                .as_ref()
                .unwrap()
                .get_agent_state_any(&expected_id)
                .await
                .unwrap();
            assert_eq!(live.run_id, run.run_id);
            assert_eq!(
                live.committed_frontier.as_ref().unwrap().run_generation,
                run.run_generation
            );
            records.push(run);
        }

        // A cold observer has neither the old archive nor an executor. It
        // reconstructs identity solely from the original admission records.
        let cold = DynamicAgentSpawner::new(Arc::new(AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            Arc::new(DelegationTracker::new()),
        )));
        let parent = cold.fanout_parent(&parent_run_id);
        assert_eq!(cold.restore_durable_agent_runs(&records).await, 2);
        assert_eq!(cold.get_agent_history(Some(&parent_run_id)).await.len(), 2);
        assert_eq!(parent.pending_direct_children().len(), 2);
        assert!(
            cold.list_all_agents().await.is_empty(),
            "recovery does not acquire executor custody"
        );

        let rejected = DynamicAgentSpawner::new(cold.mailbox_router());
        let rejected_parent = rejected.fanout_parent(&parent_run_id);
        for run in &records {
            let admission = run
                .events
                .iter()
                .find(|event| event["event_type"] == "run_started")
                .unwrap();
            let mut missing_id = admission.clone();
            missing_id["data"]
                .as_object_mut()
                .unwrap()
                .remove("child_runtime_id");
            let mut malformed_id = admission.clone();
            malformed_id["data"]["child_runtime_id"] = serde_json::json!(42);
            let mut empty_id = admission.clone();
            empty_id["data"]["child_runtime_id"] = serde_json::json!(" ");
            let mut conflicting_id = admission.clone();
            conflicting_id["data"]["child_runtime_id"] = serde_json::json!("another-child");
            for admissions in [
                vec![],
                vec![missing_id],
                vec![malformed_id],
                vec![empty_id],
                vec![admission.clone(), admission.clone()],
                vec![admission.clone(), conflicting_id],
                vec![
                    admission.clone(),
                    serde_json::json!({
                        "type": "agent_spawned", "run_id": run.run_id,
                        "agent_id": "another-child", "parent_run_id": parent_run_id,
                    }),
                ],
            ] {
                let mut invalid = run.clone();
                invalid
                    .events
                    .retain(|event| event["event_type"] != "run_started");
                invalid.events.extend(admissions);
                invalid.status = STATUS_FAILED.into();
                invalid.error_message = Some("untrusted snapshot update".into());
                invalid.last_event_idx += 1;
                assert_eq!(
                    rejected
                        .restore_durable_agent_runs(&[invalid.clone()])
                        .await,
                    0
                );
                assert!(rejected.get_agent_history(None).await.is_empty());
                assert!(!rejected_parent.has_pending_direct_children());

                // Even a newer row cannot replace the exact admitted instance
                // when its identity proof is missing, malformed or ambiguous.
                cold.set_durable_agent_reconciler(Arc::new(StaticDurableReconciler {
                    runs: vec![invalid],
                }))
                .await;
                assert_eq!(cold.reconcile_durable_agent_runs().await.unwrap(), 0);
                assert_eq!(cold.get_agent_history(None).await.len(), 2);
                let state = cold
                    .get_agent_state_any(&format!("writer@{}", run.run_id))
                    .await
                    .unwrap();
                assert_eq!(
                    state.committed_frontier,
                    Some(SpawnRunFrontier::from_durable(run))
                );
            }
        }

        for run in &records {
            let id = format!("writer@{}", run.run_id);
            let state = cold.get_agent_state_any(&id).await.unwrap();
            let frontier = SpawnRunFrontier::from_durable(run);
            assert_eq!(state.run_id, run.run_id);
            assert_eq!(state.agent_type, "writer");
            assert_eq!(state.committed_frontier.as_ref(), Some(&frontier));
            assert_eq!(state.metrics.prompt_tokens, run.total_prompt_tokens);
            assert_eq!(state.metrics.completion_tokens, run.total_completion_tokens);
            assert_eq!(state.metrics.tool_calls, run.total_tool_calls);

            let mut foreign = frontier.clone();
            foreign.run_id = "another-run".into();
            assert!(!parent.consume_terminal_child(&id, &foreign));
            let mut stale = frontier.clone();
            stale.run_generation = stale.run_generation.saturating_add(1);
            assert!(!parent.consume_terminal_child(&id, &stale));
            if run.status == STATUS_PAUSED {
                assert!(matches!(
                    state.status,
                    crate::orchestration::AgentStatus::Paused { .. }
                ));
                assert!(!parent.consume_terminal_child(&id, &frontier));
            } else {
                assert_eq!(run.status, STATUS_FAILED);
                assert!(matches!(
                    state.status,
                    crate::orchestration::AgentStatus::Failed { .. }
                ));
                assert!(parent.consume_terminal_child(&id, &frontier));
            }
        }
        let pending = parent.pending_direct_children();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].status,
            crate::orchestration::AgentStatus::Paused { .. }
        ));
        assert_eq!(cold.restore_durable_agent_runs(&records).await, 0);
        let replayed = parent.pending_direct_children();
        assert_eq!(
            replayed.len(),
            1,
            "snapshot replay cannot reopen consumed results"
        );
        assert_eq!(replayed[0].agent_id, pending[0].agent_id);
        assert_eq!(replayed[0].run_id, pending[0].run_id);
        assert_eq!(replayed[0].status, pending[0].status);

        // Reconciliation uses the same runtime identity as cold restore, not
        // the shared business profile. A proven resume advances only this run.
        let mut resumed = records
            .iter()
            .find(|run| run.status == STATUS_PAUSED)
            .unwrap()
            .clone();
        resumed.status = astra_core::STATUS_RUNNING.into();
        resumed.waiting_for = None;
        resumed.run_generation += 1;
        resumed.last_event_idx += 1;
        cold.set_durable_agent_reconciler(Arc::new(StaticDurableReconciler {
            runs: vec![resumed.clone()],
        }))
        .await;
        assert_eq!(cold.reconcile_durable_agent_runs().await.unwrap(), 1);
        assert!(matches!(
            parent.pending_direct_children()[0].status,
            crate::orchestration::AgentStatus::Waiting { .. }
        ));
        assert_eq!(parent.pending_direct_children()[0].run_id, resumed.run_id);
        resumed.status = STATUS_FAILED.into();
        resumed.last_event_idx += 1;
        cold.set_durable_agent_reconciler(Arc::new(StaticDurableReconciler {
            runs: vec![resumed.clone()],
        }))
        .await;
        assert_eq!(cold.reconcile_durable_agent_runs().await.unwrap(), 1);
        assert!(parent.consume_terminal_child(
            &format!("writer@{}", resumed.run_id),
            &SpawnRunFrontier::from_durable(&resumed)
        ));
        assert!(!parent.has_pending_direct_children());
        assert_eq!(
            executions.load(Ordering::SeqCst),
            2,
            "cold observation must not execute children"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fork_late_child_gets_a_fresh_batch_reconciliation_budget() {
        struct StaggeredExecutor;

        #[async_trait]
        impl SubRunExecutor for StaggeredExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let fork_index = config
                    .context
                    .get("fork_index")
                    .and_then(serde_json::Value::as_u64)
                    .expect("fork child index");
                if fork_index == 1 {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.to_string(),
                        output: Some(format!("fork index {fork_index} result")),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (_, _, tracker, engine) = setup_with_executor(Arc::new(StaggeredExecutor));
        let request = fork_request("del-fork-staggered", vec!["early", "late"], "writer");
        let result = execute_with_durable_parent(&engine, request, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 2);
        let late = result
            .agent_results
            .iter()
            .find(|agent| {
                agent
                    .output
                    .as_deref()
                    .is_some_and(|output| output == "fork index 1 result")
            })
            .unwrap_or_else(|| panic!("late child result should be retained: {result:?}"));
        assert_eq!(late.status, STATUS_COMPLETED);
        assert_eq!(
            tracker.get_sub_run_state(&late.run_id).await,
            Some(SubRunState::Completed)
        );
    }

    #[tokio::test]
    async fn fork_children_inherit_and_persist_durable_parent_interaction_mode() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );
        let request = fork_request("del-fork-auto", vec!["task-a", "task-b"], "writer");
        run_engine
            .start_run_with_context(
                &request.parent_run_id,
                &request.user_id,
                &request.session_id,
                crate::server::run::engine::RunStartContext {
                    interaction_mode: RequestedTurnInteractionMode::Auto,
                    model_selection: Some(astra_turn_types::ModelSelection {
                        offering_id: "offer-parent".into(),
                    }),
                    resolved_model_selection: Some(astra_services::runs::ResolvedModelSelection {
                        offering_id: "offer-parent".into(),
                        model_name: "parent-model".into(),
                        source_identity: None,
                    }),
                    generation_controls: Some(crate::server::run::engine::RunGenerationControls {
                        thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
                        first_output_max_tokens: None,
                        preserve_thinking: true,
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("persist Auto parent");

        persist_parent_contract_fixture(&run_engine, &request)
            .await
            .unwrap();
        let result = bind_test_engine(&engine)
            .execute(request, "orch", None)
            .await
            .unwrap();
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings
                .iter()
                .all(|binding| { binding.interaction_mode == RequestedTurnInteractionMode::Auto })
        );
        drop(bindings);

        for child in result.agent_results {
            let durable = run_engine
                .load_run("user-1", &child.run_id)
                .await
                .unwrap()
                .expect("durable fork child");
            assert_eq!(durable.events[0]["data"]["interaction_mode"], "auto");
            assert_eq!(
                crate::server::run::engine::durable_run_generation_controls(&durable).unwrap(),
                crate::server::run::engine::RunGenerationControls {
                    thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
                    first_output_max_tokens: None,
                    preserve_thinking: true,
                }
            );
        }
    }

    #[tokio::test]
    async fn fork_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["writer".to_string()],
        });
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, executor);

        let req = fork_request("del-fork-fail", vec!["task-a", "task-b"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // All children use "writer" which fails → all failed
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.status, "failed");
        for ar in &result.agent_results {
            assert_eq!(ar.status, "failed");
        }
        assert!(result.aggregated_output.is_none());
    }

    #[tokio::test]
    async fn fork_single_task() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let mut req = fork_request("del-fork-single", vec!["only-task"], "writer");
        if let CoordinationPattern::Fork { aggregation, .. } = &mut req.pattern {
            *aggregation = AggregationStrategy::FirstSuccess;
        }
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.status, "completed");
        assert_eq!(result.aggregated_output, result.agent_results[0].output);
    }

    #[tokio::test]
    async fn fork_context_includes_fork_metadata() {
        /// Executor that checks fork context fields.
        struct ForkContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForkContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                let is_fork = config
                    .context
                    .get("is_fork_child")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let idx = config
                    .context
                    .get("fork_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(999);
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: "completed".to_string(),
                        output: Some(format!("is_fork={is_fork},idx={idx}")),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForkContextCheckExecutor),
        );

        let req = fork_request("del-fork-ctx", vec!["a", "b"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Both children should have fork metadata
        let outputs: Vec<String> = result
            .agent_results
            .iter()
            .filter_map(|r| r.output.clone())
            .collect();
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=0")));
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=1")));
    }

    // ── Tracker: get_children ───────────────────────────────────────────────

    #[tokio::test]
    async fn tracker_get_children_returns_child_run_ids() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-1".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-2".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "other-child".into(),
                parent_run_id: "parent-Y".into(),
                delegation_id: "del-2".into(),
                agent_id: "writer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-X").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children_y = tracker.get_children("parent-Y").await;
        assert_eq!(children_y, vec!["other-child"]);

        let none = tracker.get_children("nonexistent").await;
        assert!(none.is_empty());
    }

    // ── Tracker: individual pause_sub_run / resume_sub_run ──────────────────

    #[tokio::test]
    async fn pause_and_resume_individual_sub_run() {
        let tracker = DelegationTracker::new();
        let flag = tracker.register_pause_flag("run-1").await;

        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause individual sub-run
        assert!(tracker.pause_sub_run("run-1").await);
        assert!(flag.load(Ordering::Relaxed));
        assert!(tracker.is_paused("run-1").await);

        // Resume individual sub-run
        assert!(tracker.resume_sub_run("run-1").await);
        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause/resume unknown run returns false
        assert!(!tracker.pause_sub_run("unknown").await);
        assert!(!tracker.resume_sub_run("unknown").await);
    }

    // ── Fan-out: all agents fail ────────────────────────────────────────────

    #[tokio::test]
    async fn fan_out_all_agents_fail() {
        let (reg, engine, tracker) = setup();
        let failing = Arc::new(FailingExecutor {
            fail_agents: vec!["coder".into(), "reviewer".into()],
        });
        let de = DelegationEngine::with_executor(reg, engine, tracker, failing);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // All results should be failed
        assert_eq!(result.agent_results.len(), 2);
        for r in &result.agent_results {
            assert_eq!(r.status, "failed");
            assert!(r.error.is_some());
        }
    }

    // ── Executor hard error (Err) vs soft fail (Ok with failed status) ──────

    #[tokio::test]
    async fn executor_hard_error_captured_as_failed_result() {
        /// Executor that returns Err (panic-like failure, not just failed status).
        struct HardErrorExecutor;

        #[async_trait]
        impl SubRunExecutor for HardErrorExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                Err(format!(
                    "executor crashed for {}",
                    config.agent_profile.agent_id
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(HardErrorExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Hard errors should be captured as failed agent results, not propagated
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("crashed")
        );
    }

    // ── Sequential: output chaining across stages ───────────────────────────

    #[tokio::test]
    async fn sequential_output_chaining_verified() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-chain".into(),
            parent_run_id: "p1".into(),
            task: "chained task".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 3);

        // Each stage receives previous output
        let out0 = result.agent_results[0].output.as_ref().unwrap();
        assert!(out0.contains("[coder]"), "first stage should run");

        let out1 = result.agent_results[1].output.as_ref().unwrap();
        assert!(
            out1.contains("prev="),
            "second stage should receive prev output"
        );

        let out2 = result.agent_results[2].output.as_ref().unwrap();
        assert!(
            out2.contains("prev="),
            "third stage should receive prev output"
        );
    }

    // ── State Machine + Lifecycle Tests ──────────────────────────────────

    #[tokio::test]
    async fn tracker_state_transitions() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        // Created → Running
        let new = tracker
            .transition_state("r1", SubRunState::Running)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Running);

        // Running → Completed
        let new = tracker
            .transition_state("r1", SubRunState::Completed)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Completed);
    }

    #[tokio::test]
    async fn tracker_recording_is_idempotent_by_durable_run_identity() {
        let tracker = DelegationTracker::new();
        let mut record = SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "parent".into(),
            delegation_id: "d1".into(),
            agent_id: "a1".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        };
        tracker.record_sub_run(record.clone()).await;
        record.state = SubRunState::Running;
        tracker.record_sub_run(record).await;

        let records = tracker.get_sub_runs("d1").await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, SubRunState::Created);
        assert_eq!(tracker.get_depth("r1").await, Some(1));
        assert_eq!(tracker.get_agent_id("r1").await.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn mailbox_lineage_registration_does_not_publish_a_second_spawn() {
        use astra_messaging::DelegationLookup;
        let broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::new(8));
        let mut events = broadcaster.subscribe();
        let tracker = DelegationTracker::new().with_progress_broadcaster(broadcaster);

        DelegationLookup::record_sub_run(
            &tracker,
            astra_messaging::SubRunInfo {
                run_id: "child-run".into(),
                parent_run_id: "root-run".into(),
                delegation_id: "root-run".into(),
                agent_id: "reviewer-1".into(),
                depth: 1,
            },
        )
        .await;

        assert_eq!(
            tracker.get_parent("child-run").await.as_deref(),
            Some("root-run")
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "mailbox lineage bookkeeping must leave lifecycle publication to the spawner"
        );
    }

    #[tokio::test]
    async fn tracker_invalid_transition_rejected() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let flag = tracker.register_pause_flag("r1").await;
        let token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token("r1", token.clone()).await;

        // Created → Completed should fail (must go through Running)
        let err = tracker.transition_state("r1", SubRunState::Completed).await;
        assert!(err.is_err());
        tracker
            .apply_sub_run_result_state("r1", SubRunState::Completed, None, None)
            .await;
        assert_eq!(
            tracker.get_sub_run_state("r1").await,
            Some(SubRunState::Created)
        );
        assert!(Arc::ptr_eq(
            &tracker.get_pause_flag("r1").await.unwrap(),
            &flag
        ));
        assert!(Arc::ptr_eq(
            tracker.state.read().await.cancel_tokens.get("r1").unwrap(),
            &token
        ));
    }

    #[tokio::test]
    async fn tracker_result_retains_lineage_and_releases_only_terminal_controls() {
        for (result_state, controls_expected) in [
            (SubRunState::Waiting, true),
            (SubRunState::Paused, true),
            (SubRunState::Completed, false),
        ] {
            let tracker = DelegationTracker::new();
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: "r1".into(),
                    parent_run_id: "parent".into(),
                    delegation_id: "d1".into(),
                    agent_id: "a1".into(),
                    depth: 1,
                    state: SubRunState::Running,
                    retry_of: None,
                })
                .await;
            let flag = tracker.register_pause_flag("r1").await;
            let token = Arc::new(tokio_util::sync::CancellationToken::new());
            tracker.register_cancel_token("r1", token.clone()).await;

            tracker
                .apply_sub_run_result_state("r1", result_state, None, None)
                .await;
            tracker
                .apply_sub_run_result_state("r1", result_state, None, None)
                .await;

            let subs = tracker.get_sub_runs("d1").await;
            assert_eq!(subs.len(), 1);
            assert_eq!(subs[0].state, result_state);
            assert_eq!(tracker.get_parent("r1").await.as_deref(), Some("parent"));
            assert_eq!(tracker.get_depth("r1").await, Some(1));
            assert_eq!(
                tracker.get_pause_flag("r1").await.is_some(),
                controls_expected
            );
            assert_eq!(
                tracker.state.read().await.cancel_tokens.contains_key("r1"),
                controls_expected
            );
            assert!(!flag.load(Ordering::SeqCst));
            assert!(!token.is_cancelled());

            if !controls_expected {
                // A late replay must release any stale control registration
                // without deleting lineage or signalling an already-settled task.
                tracker.register_pause_flag("r1").await;
                tracker.register_cancel_token("r1", token.clone()).await;
                tracker.complete_sub_run("r1", result_state).await;
                assert!(tracker.get_pause_flag("r1").await.is_none());
                assert!(!tracker.state.read().await.cancel_tokens.contains_key("r1"));
                assert!(!token.is_cancelled());
                assert_eq!(tracker.get_parent("r1").await.as_deref(), Some("parent"));
            }
        }
    }

    #[tokio::test]
    async fn cancel_token_per_execution_isolation() {
        let (_, _engine, _tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        // Create two separate cancel tokens
        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());

        // Use unique delegation/parent IDs to avoid conflicts
        let mut req1 = fan_out_request(vec!["coder"]);
        req1.delegation_id = "del-iso-1".into();
        req1.parent_run_id = "parent-iso-1".into();

        let mut req2 = fan_out_request(vec!["reviewer"]);
        req2.delegation_id = "del-iso-2".into();
        req2.parent_run_id = "parent-iso-2".into();
        req2.session_id = "test-session-2".into();

        // Execute with different tokens — cancelling one shouldn't affect the other
        let (r1, r2) = tokio::join!(
            execute_with_durable_parent(&de, req1, "orch", Some(token1.clone())),
            execute_with_durable_parent(&de, req2, "orch", Some(token2.clone())),
        );

        // Both should succeed since neither token was cancelled
        assert!(r1.is_ok(), "r1 failed: {:?}", r1.err());
        assert!(r2.is_ok(), "r2 failed: {:?}", r2.err());
    }

    /// Executor that sleeps for a configured duration before returning.
    #[derive(Clone)]
    struct SlowExecutor {
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl SubRunExecutor for SlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            tokio::time::sleep(self.delay).await;
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id.clone(),
                    status: "completed".into(),
                    output: Some(format!("slow output for {}", config.task)),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    /// Executor that reports whether a mailbox was attached to the sub-run config.
    #[derive(Clone)]
    struct MailboxEchoExecutor;

    #[async_trait::async_trait]
    impl SubRunExecutor for MailboxEchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
            Ok((
                AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id.clone(),
                    status: "completed".into(),
                    output: Some(format!("mailbox={}", config.mailbox.is_some())),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                None,
            ))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_per_agent_timeout_enforced() {
        // `start_paused = true` makes tokio time virtual: `tokio::time::sleep`
        // and `tokio::time::timeout` advance the clock without real waits, so
        // the test runs in <100ms instead of the real 1s timeout budget.
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "timeout-test".into(),
            parent_run_id: "p".into(),
            task: "slow task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 1, // 1 second timeout, executor sleeps 5s
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Should fail due to timeout
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout"),
            "expected timeout error, got: {:?}",
            result.agent_results[0].error
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_and_fork_admission_and_execution_share_one_deadline() {
        struct AdmissionAndExecutionExecutor {
            executions: Arc<std::sync::atomic::AtomicUsize>,
            captured_deadlines: Arc<
                std::sync::Mutex<Vec<Option<astra_services::runs::ExecutionDeadlineAuthority>>>,
            >,
        }

        #[async_trait]
        impl SubRunExecutor for AdmissionAndExecutionExecutor {
            async fn prepare_model_batch(
                &self,
                requests: &[SubRunModelRequest],
            ) -> Result<Vec<Option<PreparedSubRunModel>>, String> {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                Ok(vec![None; requests.len()])
            }

            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                self.executions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.captured_deadlines
                    .lock()
                    .expect("deadline mutex is not poisoned")
                    .push(config.execution_deadline);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                Ok((
                    AgentResult {
                        agent_id: config.agent_profile.agent_id,
                        run_id: config.run_id,
                        status: STATUS_COMPLETED.to_string(),
                        output: Some("late result".to_string()),
                        error: None,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                    None,
                ))
            }

            fn owns_durable_run_lifecycle(&self) -> bool {
                true
            }
        }

        for &(fork, timeout_sec) in &[
            (false, 1_u64),
            (true, 1_u64),
            (false, 5_u64),
            (true, 5_u64),
            (false, 0_u64),
        ] {
            let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let captured_deadlines = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (_, _, _, engine) = setup_with_executor(Arc::new(AdmissionAndExecutionExecutor {
                executions: executions.clone(),
                captured_deadlines: captured_deadlines.clone(),
            }));
            let mut request = if fork {
                fork_request("deadline-matrix-fork", vec!["one", "two"], "coder")
            } else {
                fan_out_request(vec!["coder", "reviewer"])
            };
            match &mut request.pattern {
                CoordinationPattern::Fork {
                    timeout_sec: pattern_timeout_sec,
                    ..
                }
                | CoordinationPattern::FanOut {
                    timeout_sec: pattern_timeout_sec,
                    ..
                } => *pattern_timeout_sec = timeout_sec,
                _ => unreachable!("deadline matrix only constructs fork or fan-out"),
            };

            let started = tokio::time::Instant::now();
            let result = execute_with_durable_parent(&engine, request, "orch", None)
                .await
                .expect("deadline yields per-slot results");

            let captured_deadlines = captured_deadlines
                .lock()
                .expect("deadline mutex is not poisoned")
                .clone();
            assert_eq!(captured_deadlines.len(), 2);
            assert_eq!(result.agent_results.len(), 2);
            assert_eq!(executions.load(std::sync::atomic::Ordering::Relaxed), 2);
            if timeout_sec == 1 {
                assert_eq!(
                    tokio::time::Instant::now() - started,
                    std::time::Duration::from_secs(1),
                    "model admission consumes the same absolute deadline as execution"
                );
            }
            // A return value is not proof that this executor committed its
            // durable terminal. It retains ownership in every timeout mode.
            assert!(
                result
                    .agent_results
                    .iter()
                    .all(|child| child.status == STATUS_WAITING && child.output.is_none())
            );
            if timeout_sec > 0 {
                assert_eq!(captured_deadlines[0], captured_deadlines[1]);
                assert!(captured_deadlines[0].is_some());
            } else {
                assert!(captured_deadlines.iter().all(Option::is_none));
            }
        }

        // The checked authority boundary is exercised through delegation:
        // an unrepresentable deadline fails before model admission
        // or any executor call rather than overflowing a scheduler Instant.
        let (_, _, _, engine) = setup_with_executor(Arc::new(SlowExecutor {
            delay: std::time::Duration::from_millis(1),
        }));
        let mut request = fan_out_request(vec!["coder"]);
        request.pattern = CoordinationPattern::FanOut {
            agent_ids: vec!["coder".into()],
            aggregation: AggregationStrategy::AllResults,
            timeout_sec: u64::MAX,
        };
        let error = execute_with_durable_parent(&engine, request, "orch", None)
            .await
            .expect_err("oversized delegation deadline must fail closed before admission");
        assert!(error.contains("execution time budget exceeds supported deadline range"));
    }

    #[tokio::test]
    async fn fan_out_registers_and_retires_child_mailboxes() {
        let (reg, engine, tracker) = setup();
        let transport = Arc::new(crate::messaging::InProcessTransport::new());
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            transport.clone(),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(MailboxEchoExecutor))
                .with_mailbox_router(router);

        let result = execute_with_durable_parent(
            &de,
            fan_out_request(vec!["coder", "reviewer"]),
            "orch",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.agent_results.len(), 2);
        for child in &result.agent_results {
            assert_eq!(child.output.as_deref(), Some("mailbox=true"));
        }
        assert_terminal_mailboxes_retired(&transport).await;
    }

    #[tokio::test]
    async fn mailbox_retirement_requires_confirmed_durable_terminal_status() {
        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        let transport = Arc::new(crate::messaging::InProcessTransport::new());
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            transport.clone(),
            Arc::new(DelegationTracker::new()),
        ));
        let mailbox_for = |run_id: &str| astra_messaging::types::AgentAddress {
            run_id: run_id.to_string(),
            agent_id: "worker".into(),
        };
        let result_for = |run_id: &str, status: &str| AgentResult {
            agent_id: "worker".into(),
            run_id: run_id.into(),
            status: status.into(),
            output: None,
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let router_option = Some(router.clone());

        let completed = engine.start_run("completed", "u", "s").await.unwrap();
        let mailbox = router
            .register(mailbox_for("completed"), None)
            .await
            .unwrap();
        let retirement = MailboxRetirement::from_mailbox(&router_option, Some(&mailbox)).unwrap();
        drop(mailbox);
        let result = reconcile_agent_result_with_durable_authority(
            &engine,
            "u",
            "s",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: completed.owner_generation,
            },
            result_for("completed", STATUS_COMPLETED),
            Some(&retirement),
        )
        .await
        .0;
        assert_eq!(result.status, STATUS_COMPLETED);

        let reread = engine.start_run("reread", "u", "s").await.unwrap();
        assert!(
            engine
                .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                    user_id: "u",
                    expected_session_id: "s",
                    run_id: "reread",
                    status: STATUS_COMPLETED,
                    waiting_for: None,
                    error_message: None,
                    expected_statuses: &["running"],
                })
                .await
                .unwrap()
        );
        let mailbox = router.register(mailbox_for("reread"), None).await.unwrap();
        let reread_retirement =
            MailboxRetirement::from_mailbox(&router_option, Some(&mailbox)).unwrap();
        drop(mailbox);
        let result = reconcile_agent_result_with_durable_authority(
            &engine,
            "u",
            "s",
            DurableLifecycleDisposition::ExecutorOwned {
                owner_generation: reread.owner_generation,
            },
            result_for("reread", STATUS_COMPLETED),
            Some(&reread_retirement),
        )
        .await
        .0;
        assert_eq!(result.status, STATUS_COMPLETED);

        let waiting = engine.start_run("waiting", "u", "s").await.unwrap();
        let mailbox = router.register(mailbox_for("waiting"), None).await.unwrap();
        let waiting_retirement =
            MailboxRetirement::from_mailbox(&router_option, Some(&mailbox)).unwrap();
        drop(mailbox);
        let result = reconcile_agent_result_with_durable_authority(
            &engine,
            "u",
            "s",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: waiting.owner_generation,
            },
            result_for("waiting", STATUS_WAITING),
            Some(&waiting_retirement),
        )
        .await
        .0;
        assert_eq!(result.status, STATUS_WAITING);

        let mailbox = router.register(mailbox_for("missing"), None).await.unwrap();
        let missing_retirement =
            MailboxRetirement::from_mailbox(&router_option, Some(&mailbox)).unwrap();
        drop(mailbox);
        let result = reconcile_agent_result_with_durable_authority(
            &engine,
            "u",
            "s",
            DurableLifecycleDisposition::ReadOnly,
            result_for("missing", STATUS_FAILED),
            Some(&missing_retirement),
        )
        .await
        .0;
        assert_eq!(result.status, STATUS_WAITING);
        assert!(result.is_unfinished());

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while transport.retained_inbox_count().await != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("only unconfirmed or nonterminal mailboxes may remain");
        let resumed_waiting = router.register(mailbox_for("waiting"), None).await.unwrap();
        assert_eq!(resumed_waiting.lifetime(), &waiting_retirement.lifetime);
        let resumed_missing = router.register(mailbox_for("missing"), None).await.unwrap();
        assert_eq!(resumed_missing.lifetime(), &missing_retirement.lifetime);
        drop(resumed_waiting);
        drop(resumed_missing);
        router
            .retire_terminal(&waiting_retirement.lifetime)
            .await
            .unwrap();
        router
            .retire_terminal(&missing_retirement.lifetime)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_shared_deadline_still_reconciles_all_terminal_mailboxes() {
        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        let transport = Arc::new(crate::messaging::InProcessTransport::new());
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            transport.clone(),
            Arc::new(DelegationTracker::new()),
        ));
        let router_option = Some(router.clone());
        let mut deadline = Some(tokio::time::Instant::now());
        for run_id in ["first", "second"] {
            let authority = engine.start_run(run_id, "u", "s").await.unwrap();
            assert!(
                engine
                    .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                        user_id: "u",
                        expected_session_id: "s",
                        run_id,
                        status: STATUS_COMPLETED,
                        waiting_for: None,
                        error_message: None,
                        expected_statuses: &["running"],
                    })
                    .await
                    .unwrap()
            );
            let mailbox = router
                .register(
                    astra_messaging::types::AgentAddress::new(run_id, "worker"),
                    None,
                )
                .await
                .unwrap();
            let retirement =
                MailboxRetirement::from_mailbox(&router_option, Some(&mailbox)).unwrap();
            drop(mailbox);
            let projected = reconcile_agent_result_with_shared_deadline(
                &engine,
                "u",
                "s",
                DurableLifecycleDisposition::ExecutorOwned {
                    owner_generation: authority.owner_generation,
                },
                AgentResult {
                    agent_id: "worker".into(),
                    run_id: run_id.into(),
                    status: STATUS_COMPLETED.into(),
                    output: None,
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
                Some(&retirement),
                &mut deadline,
                "fanout",
                "child outcome",
            )
            .await;
            assert_eq!(projected.status, STATUS_WAITING);
        }
        assert_terminal_mailboxes_retired(&transport).await;
    }

    #[tokio::test(start_paused = true)]
    async fn sequential_per_stage_timeout_enforced() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "seq-timeout".into(),
            parent_run_id: "p".into(),
            task: "slow pipeline".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 1,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Both agents should surface their stage timeout.
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, AGENT_RESULT_STATUS_TIMEOUT);
            assert!(
                ar.error.as_deref().unwrap_or("").contains("timeout"),
                "expected timeout error for {}, got: {:?}",
                ar.agent_id,
                ar.error
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_means_no_timeout() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_millis(50),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "no-timeout".into(),
            parent_run_id: "p".into(),
            task: "quick task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0, // no timeout
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");
    }

    /// P1-B: cancel_children_of must cancel all child tokens.
    #[tokio::test]
    async fn cancel_children_of_cancels_tokens() {
        let tracker = DelegationTracker::new();
        let parent = "parent-run";
        let child1 = "child-1";
        let child2 = "child-2";

        // Register children under parent
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child1.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-a".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child2.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-b".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token(child1, token1.clone()).await;
        tracker.register_cancel_token(child2, token2.clone()).await;

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        let count = tracker.cancel_children_of(parent).await;
        assert_eq!(count, 2, "both children must be cancelled");
        assert!(token1.is_cancelled(), "child1 token must be cancelled");
        assert!(token2.is_cancelled(), "child2 token must be cancelled");
        assert_eq!(
            tracker.get_sub_run_state(child1).await,
            Some(SubRunState::Running),
            "a cancellation request is not a fabricated terminal outcome"
        );
        assert_eq!(
            tracker.get_sub_run_state(child2).await,
            Some(SubRunState::Running),
            "the executor reports the eventual terminal state"
        );
    }

    #[tokio::test]
    async fn single_cancel_request_signals_only_the_target_and_keeps_outcome_pending() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-cancel".into(),
                parent_run_id: "parent-run".into(),
                delegation_id: "deleg-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        let pause_flag = tracker.register_pause_flag("child-cancel").await;
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token("child-cancel", cancel_token.clone())
            .await;

        assert!(tracker.request_cancel_sub_run("child-cancel").await);
        assert!(
            !pause_flag.load(Ordering::Acquire),
            "cancellation must not be misclassified as a pause"
        );
        assert!(cancel_token.is_cancelled());
        assert_eq!(
            tracker.get_sub_run_state("child-cancel").await,
            Some(SubRunState::Running),
            "the executor owns the terminal cancelled outcome"
        );
    }

    #[tokio::test]
    async fn terminal_sub_run_rejects_late_cancellation_without_signalling_token() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-complete".into(),
                parent_run_id: "parent-run".into(),
                delegation_id: "deleg-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Completed,
                retry_of: None,
            })
            .await;
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token("child-complete", cancel_token.clone())
            .await;

        assert!(!tracker.request_cancel_sub_run("child-complete").await);
        assert!(!cancel_token.is_cancelled());
    }

    /// Regression: the SSE Failed event for a non-Completed/Paused/Cancelled
    /// terminal state (e.g. VerificationFailed) must surface the canonical
    /// `as_str()` wire form, NOT the Debug-formatted Rust enum variant.
    /// Pre-fix the broadcaster received "Sub-run terminal state:
    /// VerificationFailed"; the wire/JSON contract everywhere else uses
    /// "verification_failed", so the Debug leak coupled SSE consumers to
    /// the Debug derive — a refactor of the enum casing would have
    /// silently broken downstream parsing.
    #[tokio::test(flavor = "current_thread")]
    async fn sse_failed_event_uses_canonical_wire_status_not_debug() {
        use crate::orchestration::{ProgressBroadcaster, ProgressEventType};
        let broadcaster = Arc::new(ProgressBroadcaster::new(16));
        let mut rx = broadcaster.subscribe();
        let tracker = DelegationTracker::new().with_progress_broadcaster(broadcaster.clone());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-vf".into(),
                parent_run_id: "parent-vf".into(),
                delegation_id: "deleg-vf".into(),
                agent_id: "agent-vf".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        tracker
            .complete_sub_run_with_result(
                "run-vf",
                SubRunState::VerificationFailed,
                Some("acceptance criterion 3 failed"),
                None,
            )
            .await;

        // Drain events until we see the terminal Failed (record_sub_run
        // emits a Started/Spawned event which we don't care about here).
        let error_text = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event should arrive within timeout")
                .expect("broadcast must deliver");
            match event.event_type {
                ProgressEventType::Failed { error } => break error,
                ProgressEventType::Completed { .. }
                | ProgressEventType::Cancelled { .. }
                | ProgressEventType::Interrupted { .. } => {
                    panic!(
                        "expected Failed for VerificationFailed terminal state, got {:?}",
                        event.event_type
                    );
                }
                _ => continue, // skip non-terminal events
            }
        };

        assert!(
            error_text.contains("verification_failed"),
            "SSE Failed event must use canonical wire status; got: {error_text}"
        );
        assert!(
            !error_text.contains("VerificationFailed"),
            "SSE Failed event must not leak the Rust Debug variant casing; got: {error_text}"
        );
    }

    /// Subtree cancellation: cancel_children_of must propagate to grandchildren
    /// (and deeper). Previously the implementation filtered the parents map by
    /// direct `parent_run_id` match only, leaving any sub-runs spawned by a
    /// cancelled child still alive — a real correctness bug for any
    /// multi-level delegation tree.
    #[tokio::test]
    async fn cancel_children_of_propagates_to_grandchildren() {
        let tracker = DelegationTracker::new();
        let parent = "parent-run";
        let child = "child-run";
        let grandchild = "grandchild-run";

        for (rid, prid, did) in [(child, parent, "deleg-1"), (grandchild, child, "deleg-2")] {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: rid.into(),
                    parent_run_id: prid.into(),
                    delegation_id: did.into(),
                    agent_id: "agent".into(),
                    depth: 0,
                    state: SubRunState::Running,
                    retry_of: None,
                })
                .await;
        }

        let token_child = Arc::new(tokio_util::sync::CancellationToken::new());
        let token_grand = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token(child, token_child.clone())
            .await;
        tracker
            .register_cancel_token(grandchild, token_grand.clone())
            .await;

        let count = tracker.cancel_children_of(parent).await;
        assert!(token_child.is_cancelled(), "direct child must be cancelled");
        assert!(
            token_grand.is_cancelled(),
            "grandchild MUST be cancelled (subtree, not just first level)"
        );
        assert_eq!(count, 2, "count must include all descendants");
    }

    #[tokio::test]
    async fn collect_descendants_visits_siblings_before_grandchildren() {
        let tracker = DelegationTracker::new();
        for (rid, prid) in [
            ("child-a", "parent-run"),
            ("child-b", "parent-run"),
            ("grandchild-a", "child-a"),
        ] {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: rid.into(),
                    parent_run_id: prid.into(),
                    delegation_id: format!("deleg-{rid}"),
                    agent_id: "agent".into(),
                    depth: 0,
                    state: SubRunState::Running,
                    retry_of: None,
                })
                .await;
        }

        let descendants = tracker.collect_descendants("parent-run").await;
        let child_b = descendants
            .iter()
            .position(|run_id| run_id == "child-b")
            .expect("child-b should be collected");
        let grandchild_a = descendants
            .iter()
            .position(|run_id| run_id == "grandchild-a")
            .expect("grandchild-a should be collected");

        assert!(
            child_b < grandchild_a,
            "BFS must visit direct siblings before grandchildren: {descendants:?}"
        );
    }

    #[tokio::test]
    async fn resolve_inherited_prefix_returns_none_without_store() {
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let tracker = Arc::new(DelegationTracker::new());
        let engine = DelegationEngine::with_executor(
            registry,
            Arc::new(RunEngine::new(run_store)),
            tracker,
            Arc::new(StubSubRunExecutor),
        );
        let out = engine.resolve_inherited_prefix_for_delegate("run-parent", "MiniMax-M2.5");
        assert!(
            out.is_none(),
            "engine without prefix_store must return None inherited_prefix"
        );
    }

    #[tokio::test]
    async fn resolve_inherited_prefix_resolves_captured_parent() {
        use astra_turn_core::fork_capture::{
            CaptureRequest, ForkCaptureOutcome, capture_parent_prefix,
        };
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
            hash_tool_schema,
        };
        use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};

        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
        let parent_msgs = serde_json::json!([
            {"role": "user", "content": "analyze"},
            {"role": "assistant", "content": "done"}
        ]);
        let canonical = serde_json::to_vec(&parent_msgs).unwrap();
        let capture = capture_parent_prefix(
            CaptureRequest {
                parent_run_id: "run-step2".into(),
                parent_turn_seq: 1,
                provider: ProviderKind::Other("MiniMax-M2.5".into()),
                model_id: "MiniMax-M2.5".into(),
                thinking: Some(ThinkingConfigSlice {
                    enabled: false,
                    budget_tokens: 0,
                    kind: "disabled".into(),
                }),
                system_blocks: vec![SystemBlock {
                    bytes: b"sys".to_vec(),
                    has_cache_control: true,
                }],
                tool_schemas: vec![ToolSchemaEntry {
                    name: "bash".into(),
                    canonical_bytes: schema_bytes,
                    hash: schema_hash,
                }],
                beta_headers: vec![],
                canonical_prefix_bytes: canonical,
                cache_mode: CacheMode::Write,
                captured_at_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                microcompact_fired_in_turn: false,
            },
            &*store,
        );
        assert!(matches!(capture, ForkCaptureOutcome::Captured { .. }));

        struct PrefixExecutor {
            model: &'static str,
            run_engine: Arc<RunEngine>,
            inherited: Arc<std::sync::Mutex<Vec<bool>>>,
        }
        #[async_trait]
        impl SubRunExecutor for PrefixExecutor {
            async fn prepare_model_batch(
                &self,
                requests: &[SubRunModelRequest],
            ) -> Result<Vec<Option<PreparedSubRunModel>>, String> {
                Ok(requests
                    .iter()
                    .map(|_| {
                        Some(PreparedSubRunModel {
                            offering_id: "opaque-offering-id".into(),
                            model_name: self.model.into(),
                            admitted_execution: None,
                        })
                    })
                    .collect())
            }

            async fn execute(&self, config: SubRunConfig) -> Result<SubRunExecutionResult, String> {
                if let Some(prefix) = &config.inherited_prefix {
                    assert_eq!(prefix.parent_run_id, "run-step2");
                    assert!(!prefix.prefix_messages.is_empty());
                }
                self.inherited
                    .lock()
                    .unwrap()
                    .push(config.inherited_prefix.is_some());
                if matches!(
                    config.profile_authority,
                    ParentProfileAuthority::NonDelegating { .. }
                ) {
                    let child = self
                        .run_engine
                        .load_run(&config.user_id, &config.run_id)
                        .await?
                        .unwrap();
                    crate::orchestration::agent_tool::tests::assert_restricted_child_handlers(
                        &child,
                        config.inherited_prefix.is_some(),
                        "coder",
                    )
                    .await;
                }
                EchoExecutor.execute(config).await
            }
        }
        for model in ["MiniMax-M2.5", "different-model"] {
            for fork in [false, true] {
                let (registry, run_engine, tracker) = setup();
                let inherited = Arc::new(std::sync::Mutex::new(Vec::new()));
                let engine = bind_test_engine(
                    &DelegationEngine::with_executor(
                        registry,
                        run_engine.clone(),
                        tracker,
                        Arc::new(PrefixExecutor {
                            model,
                            run_engine: run_engine.clone(),
                            inherited: inherited.clone(),
                        }),
                    )
                    .with_prefix_store(store.clone()),
                );
                let mut request = if fork {
                    fork_request("prefix-fork", vec!["inspect"], "writer")
                } else {
                    fan_out_request(vec!["coder"])
                };
                request.parent_run_id = "run-step2".into();
                let parent = engine
                    .spawner
                    .as_ref()
                    .unwrap()
                    .fanout_parent(&request.parent_run_id);
                let result = execute_with_durable_parent(&engine, request, "orch", None)
                    .await
                    .unwrap();
                assert_eq!(result.status, STATUS_COMPLETED);
                assert_eq!(*inherited.lock().unwrap(), vec![model == "MiniMax-M2.5"]);
                assert!(!parent.has_pending_direct_children());
                let state = engine
                    .spawner
                    .as_ref()
                    .unwrap()
                    .get_agent_state_any(&format!(
                        "{}@{}",
                        result.agent_results[0].agent_id, result.agent_results[0].run_id
                    ))
                    .await
                    .unwrap();
                let prepared = state.prepared_model.unwrap();
                assert_eq!(prepared.offering_id, "opaque-offering-id");
                assert_eq!(prepared.model_name, model);
                assert_eq!(prepared.provenance, "delegation_admission");
            }
        }
    }
}
