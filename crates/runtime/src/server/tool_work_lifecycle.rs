use astra_server_types::{
    WORK_TASK_BOARD_TEXT_MAX_BYTES, WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION, WorkCreateRequestV1,
    WorkTaskBoardBlockerKindV1, WorkTaskBoardChangeV1, WorkTaskBoardDeclarationStateV1,
    WorkTaskBoardDeliveryStatusV1, WorkTaskBoardExecutionStatusV1, WorkTaskBoardTaskV1,
    WorkTaskBoardUpdateV1,
};
use astra_services::work::{
    DatabaseWorkEstablishmentService, InternalSessionId, NewWorkAttemptSettlement,
    NewWorkItemAttempt, WorkAttemptExecutionMode, WorkAttemptOutcome, WorkEstablishmentActivation,
    WorkEstablishmentPhase, WorkEstablishmentRequest, WorkEstablishmentState, WorkItemAttemptId,
    WorkItemDeclarationState, WorkItemDeliveryStatus, WorkItemExecutionStatus, WorkItemId,
    WorkItemRevision, WorkItemRevisionRef, WorkItemText, WorkOwnerId, WorkRepository,
    WorkRepositoryError, WorkTaskExecutionNext, WorkTaskExecutionSnapshot,
};
use astra_tools::ToolResult;
use astra_tools::tool_engine::ToolInvocationMetadata;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const WORK_ERROR_KIND_NOT_BOUND: &str = "work_not_bound";
const WORK_ERROR_KIND_BINDING_CHANGED: &str = "work_binding_changed";

/// Return a typed Work precondition failure.
///
/// Work lifecycle tools are state transitions, so a missing binding is not an
/// opaque executor error.  The model needs the same durable fact the runtime
/// used to reject the transition and the one legal next transition.  Keep the
/// human-readable message in the JSON payload for providers that only expose
/// tool text, while duplicating the stable fields in metadata for the journal,
/// stream renderer, and policy reducer.
fn work_precondition_error(
    error_kind: &'static str,
    message: impl Into<String>,
    next_action: &'static str,
) -> ToolResult {
    let message = message.into();
    let output = json!({
        "status": "rejected",
        "error_kind": error_kind,
        "error": message,
        "retryable": false,
        "next_action": next_action,
    })
    .to_string();
    let metadata = serde_json::Map::from_iter([
        ("status".to_string(), Value::String("rejected".to_string())),
        (
            "error_kind".to_string(),
            Value::String(error_kind.to_string()),
        ),
        ("retryable".to_string(), Value::Bool(false)),
        ("blocked".to_string(), Value::Bool(true)),
        ("execution_started".to_string(), Value::Bool(false)),
        ("side_effects_maybe".to_string(), Value::Bool(false)),
        (
            "next_action".to_string(),
            Value::String(next_action.to_string()),
        ),
        (
            "result_class".to_string(),
            Value::String("execution_error".to_string()),
        ),
    ]);
    ToolResult {
        output,
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

async fn defer_pending_work_establishment(
    executor: &RuntimeToolExecutor,
    establishment: &DatabaseWorkEstablishmentService,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    operation_id: &str,
    run_id: &str,
) -> ToolResult {
    let operation = match establishment.load(owner_id, operation_id).await {
        Ok(operation) => operation,
        Err(error) => {
            return ToolResult::error(format!(
                "pending Work establishment could not be loaded for defer: {error}"
            ));
        }
    };
    if &operation.session_id != session_id {
        return work_precondition_error(
            "work_establishment_binding_changed",
            "pending Work establishment belongs to another session",
            "refresh_work_state",
        );
    }
    if operation.state == WorkEstablishmentState::Complete {
        return work_precondition_error(
            "work_establishment_already_started",
            "Work establishment already committed its activation; use the active-attempt control",
            "inspect_work_plan",
        );
    }
    let Some(pool) = executor.context_manifest_pool.clone() else {
        return ToolResult::error("canonical Work storage is unavailable".to_string());
    };
    let repository = astra_services::work::DatabaseWorkRepository::new(pool);
    match repository
        .load_task_execution_snapshot_for_session(owner_id, session_id)
        .await
    {
        Ok(snapshot)
            if snapshot.items().iter().any(|item| {
                matches!(
                    item.execution.status,
                    WorkItemExecutionStatus::Running | WorkItemExecutionStatus::Paused
                )
            }) =>
        {
            return work_precondition_error(
                "work_establishment_already_assigned",
                "Work already has a durable active attempt; establishment defer cannot cancel it",
                "inspect_work_plan",
            );
        }
        Ok(_) | Err(WorkRepositoryError::NotFound) => {}
        Err(error) => {
            return ToolResult::error(format!(
                "pending Work establishment could not verify assignment state: {error}"
            ));
        }
    }
    let request = WorkEstablishmentRequest {
        operation_id: operation.operation_id.clone(),
        request_hash: operation.request_hash.clone(),
        payload_json: operation.payload_json.clone(),
        owner_id: owner_id.clone(),
        work_id: operation.work_id.clone(),
        branch_id: operation.branch_id.clone(),
        session_id: operation.session_id.clone(),
        run_id: run_id.to_string(),
        activation: operation.activation,
    };
    let cancelled = match establishment
        .cancel(
            &request,
            "superseded by a typed defer before Work activation",
        )
        .await
    {
        Ok(operation) => operation,
        Err(error) => {
            return ToolResult::error(format!(
                "pending Work establishment could not be deferred: {error}"
            ));
        }
    };
    if cancelled.state != WorkEstablishmentState::Cancelled {
        return work_precondition_error(
            "work_establishment_defer_conflict",
            format!(
                "pending Work establishment reached terminal state {:?} before defer",
                cancelled.state
            ),
            "refresh_work_state",
        );
    }
    ToolResult::text(
        json!({
            "status": "deferred",
            "operation_id": cancelled.operation_id,
            "operation_state": cancelled.state,
            "assignment_created": false,
        })
        .to_string(),
    )
}

use super::runtime_tool_executor::{
    ActivePrimaryWorkAttempt, RuntimeToolExecutor, WorkEstablishmentInvocation, WorkRuntimeBinding,
};

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct StartWorkArgs {
    goal: String,
    activation: StartWorkActivation,
    tasks: Vec<InitialWorkTask>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StartWorkActivation {
    Start,
    Defer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct InitialWorkItem {
    item_id: String,
    kind: &'static str,
    objective: String,
    expected_result: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct InitialWorkTask {
    objective: String,
    expected_result: String,
}

#[derive(Serialize)]
struct InitialWorkDependency {
    predecessor_item_id: String,
    successor_item_id: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct InitialRunnableItem {
    item_id: String,
    item_revision: i64,
}

/// A server-issued identity receipt for one task in the initial ordered list.
/// The model never supplies these identities; they make the durable allocation
/// explicit to consumers that need to verify a later assignment.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct InitialDeclaredTask {
    item_id: String,
    item_revision: i64,
}

/// Produce a UTF-8-safe display summary for the event-stream board. Full
/// task text remains available from the canonical graph observer. This is a
/// transport budget, not a semantic transformation or an LLM-facing rewrite.
fn task_board_display_text(text: &str) -> String {
    if text.len() <= WORK_TASK_BOARD_TEXT_MAX_BYTES {
        return text.to_string();
    }
    let mut end = WORK_TASK_BOARD_TEXT_MAX_BYTES.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn board_task_for_active_attempt(
    active: &ActivePrimaryWorkAttempt,
    execution_status: WorkTaskBoardExecutionStatusV1,
    delivery_status: WorkTaskBoardDeliveryStatusV1,
    delivery_summary: Option<String>,
    blocker_kind: Option<WorkTaskBoardBlockerKindV1>,
    unavailable_capabilities: Vec<String>,
) -> WorkTaskBoardTaskV1 {
    WorkTaskBoardTaskV1 {
        item_id: active.item_id.clone(),
        item_revision: active.item_revision,
        objective: task_board_display_text(&active.objective),
        expected_result: task_board_display_text(&active.expected_result),
        declaration_state: WorkTaskBoardDeclarationStateV1::Active,
        execution_status,
        delivery_status,
        delivery_summary,
        blocker_kind,
        unavailable_capabilities,
    }
}

fn board_blocker_kind(
    blocker_kind: Option<astra_services::work::WorkAttemptBlockerKind>,
) -> Option<WorkTaskBoardBlockerKindV1> {
    blocker_kind.map(|kind| match kind {
        astra_services::work::WorkAttemptBlockerKind::CapabilityUnavailable => {
            WorkTaskBoardBlockerKindV1::CapabilityUnavailable
        }
        astra_services::work::WorkAttemptBlockerKind::DependencyBlocked => {
            WorkTaskBoardBlockerKindV1::DependencyBlocked
        }
        astra_services::work::WorkAttemptBlockerKind::PolicyBlocked => {
            WorkTaskBoardBlockerKindV1::PolicyBlocked
        }
        astra_services::work::WorkAttemptBlockerKind::ExternalUnavailable => {
            WorkTaskBoardBlockerKindV1::ExternalUnavailable
        }
    })
}

fn board_settled_task(
    active: &ActivePrimaryWorkAttempt,
    recorded: &astra_services::work::RecordedWorkAttemptSettlement,
) -> WorkTaskBoardTaskV1 {
    let (execution_status, delivery_status) = match recorded.outcome {
        WorkAttemptOutcome::Delivered => (
            WorkTaskBoardExecutionStatusV1::Completed,
            WorkTaskBoardDeliveryStatusV1::Delivered,
        ),
        WorkAttemptOutcome::Blocked => (
            WorkTaskBoardExecutionStatusV1::Completed,
            WorkTaskBoardDeliveryStatusV1::Blocked,
        ),
        WorkAttemptOutcome::Failed => (
            WorkTaskBoardExecutionStatusV1::Failed,
            WorkTaskBoardDeliveryStatusV1::Failed,
        ),
    };
    board_task_for_active_attempt(
        active,
        execution_status,
        delivery_status,
        Some(task_board_display_text(&recorded.summary)),
        board_blocker_kind(recorded.blocker_kind),
        recorded.unavailable_capabilities.clone(),
    )
}

/// Compact, model-facing authority for the exact item transition recorded by
/// settlement. The free-form summary is intentionally excluded: it may
/// explain progress, but it cannot redefine declaration, execution, or
/// delivery state.
fn canonical_settlement_transition(task: &WorkTaskBoardTaskV1) -> Value {
    json!({
        "authority": "canonical_work_state",
        "item_id": task.item_id,
        "item_revision": task.item_revision,
        "declaration_state": task.declaration_state,
        "execution_status": task.execution_status,
        "delivery_status": task.delivery_status,
        "summary_authority": "non_authoritative_progress_note",
    })
}

fn board_update(
    work_id: String,
    branch_id: String,
    graph_revision: Option<i64>,
    tasks: Vec<WorkTaskBoardTaskV1>,
) -> WorkTaskBoardUpdateV1 {
    WorkTaskBoardUpdateV1 {
        schema_version: WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION,
        work_id,
        branch_id,
        change: WorkTaskBoardChangeV1::Upsert {
            graph_revision,
            tasks,
        },
    }
}

fn canonical_board_tasks(snapshot: &WorkTaskExecutionSnapshot) -> Vec<WorkTaskBoardTaskV1> {
    snapshot
        .items()
        .iter()
        .map(|item| WorkTaskBoardTaskV1 {
            item_id: item.item_id.as_str().to_string(),
            item_revision: item.revision.get(),
            objective: task_board_display_text(item.objective.as_str()),
            expected_result: task_board_display_text(item.expected_result.as_str()),
            declaration_state: match item.declaration_state {
                WorkItemDeclarationState::Active => WorkTaskBoardDeclarationStateV1::Active,
                WorkItemDeclarationState::Superseded => WorkTaskBoardDeclarationStateV1::Superseded,
                WorkItemDeclarationState::Cancelled => WorkTaskBoardDeclarationStateV1::Cancelled,
            },
            execution_status: match item.execution.status {
                WorkItemExecutionStatus::NotStarted => WorkTaskBoardExecutionStatusV1::NotStarted,
                WorkItemExecutionStatus::Running => WorkTaskBoardExecutionStatusV1::Running,
                WorkItemExecutionStatus::Waiting => WorkTaskBoardExecutionStatusV1::Waiting,
                WorkItemExecutionStatus::Paused => WorkTaskBoardExecutionStatusV1::Paused,
                WorkItemExecutionStatus::Completed => WorkTaskBoardExecutionStatusV1::Completed,
                WorkItemExecutionStatus::Delegated => WorkTaskBoardExecutionStatusV1::Delegated,
                WorkItemExecutionStatus::Failed => WorkTaskBoardExecutionStatusV1::Failed,
                WorkItemExecutionStatus::Cancelled => WorkTaskBoardExecutionStatusV1::Cancelled,
            },
            delivery_status: match item.delivery.status {
                WorkItemDeliveryStatus::Unreported => WorkTaskBoardDeliveryStatusV1::Unreported,
                WorkItemDeliveryStatus::Delivered => WorkTaskBoardDeliveryStatusV1::Delivered,
                WorkItemDeliveryStatus::Blocked => WorkTaskBoardDeliveryStatusV1::Blocked,
                WorkItemDeliveryStatus::Failed => WorkTaskBoardDeliveryStatusV1::Failed,
            },
            delivery_summary: item
                .delivery
                .summary
                .as_deref()
                .map(task_board_display_text),
            blocker_kind: board_blocker_kind(item.delivery.blocker_kind),
            unavailable_capabilities: item.delivery.unavailable_capabilities.clone(),
        })
        .collect()
}

async fn canonical_task_board_update(
    binding: &WorkRuntimeBinding,
    goal: Option<&str>,
    affected_item_ids: Option<&HashSet<String>>,
) -> Result<WorkTaskBoardUpdateV1, ToolResult> {
    let snapshot = binding
        .repository
        .load_task_execution_snapshot_for_session(&binding.owner_id, &binding.session_id)
        .await
        .map_err(|error| {
            ToolResult::error(format!(
                "canonical Work was committed but its task board could not be read: {error}"
            ))
        })?;
    task_board_update_from_snapshot(binding, &snapshot, goal, affected_item_ids)
}

fn task_board_update_from_snapshot(
    binding: &WorkRuntimeBinding,
    snapshot: &WorkTaskExecutionSnapshot,
    goal: Option<&str>,
    affected_item_ids: Option<&HashSet<String>>,
) -> Result<WorkTaskBoardUpdateV1, ToolResult> {
    if snapshot.basis().work_id != binding.work_id
        || snapshot.basis().branch_id != binding.branch_id
    {
        return Err(ToolResult::error(
            "canonical task-board snapshot conflicts with the session Work binding".to_string(),
        ));
    }
    let mut tasks = canonical_board_tasks(snapshot);
    if let Some(affected_item_ids) = affected_item_ids {
        tasks.retain(|task| affected_item_ids.contains(&task.item_id));
    }
    Ok(match goal {
        Some(goal) => WorkTaskBoardUpdateV1 {
            schema_version: WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION,
            work_id: binding.work_id.as_str().to_string(),
            branch_id: binding.branch_id.as_str().to_string(),
            change: WorkTaskBoardChangeV1::Snapshot {
                goal: task_board_display_text(goal),
                graph_revision: snapshot.basis().graph_revision.get(),
                criteria_member_count: 0,
                tasks,
            },
        },
        None => board_update(
            binding.work_id.as_str().to_string(),
            binding.branch_id.as_str().to_string(),
            Some(snapshot.basis().graph_revision.get()),
            tasks,
        ),
    })
}

struct AppliedGraphMutations {
    graph_revision: i64,
    affected_item_ids: Vec<String>,
}

pub(super) fn active_primary_attempt_board_event(
    executor: &RuntimeToolExecutor,
    execution_status: WorkTaskBoardExecutionStatusV1,
) -> Option<Value> {
    let binding = executor.work_binding.get()?;
    let active = executor.active_primary_work_attempt()?;
    Some(json!({
        "type": "work_task_board_update",
        "session_id": executor.session_id,
        "task_board_update": board_update(
            binding.work_id.as_str().to_string(),
            binding.branch_id.as_str().to_string(),
            None,
            vec![board_task_for_active_attempt(
                &active,
                execution_status,
                WorkTaskBoardDeliveryStatusV1::Unreported,
                None,
                None,
                Vec::new(),
            )],
        ),
    }))
}

/// Compile the user-visible initial task list into the internal execution
/// graph. IDs and task kind are server-owned; the model supplies only the
/// semantic work and its expected result.
///
/// Semantic admission does not declare precedence. Leaving dependencies empty
/// preserves that fact instead of inventing a serial relationship that would
/// misrepresent the user's outcomes and prevent a later genuine parallel
/// execution boundary. The primary scheduler still dispatches one foreground
/// task at a time by its own resource policy.
fn compile_initial_task_graph(
    tasks: &[InitialWorkTask],
) -> (Vec<InitialWorkItem>, Vec<InitialWorkDependency>) {
    let items = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| InitialWorkItem {
            item_id: format!("task-{}", index + 1),
            kind: "task",
            objective: task.objective.clone(),
            expected_result: task.expected_result.clone(),
        })
        .collect::<Vec<_>>();
    let dependencies = Vec::new();
    (items, dependencies)
}

/// Allocate retry-stable follow-up task identities in the server-owned task
/// namespace.
///
/// A second `start_work` call is a continuation of the session's existing
/// Work branch, not a request to recreate genesis. Existing plans may contain
/// A continuation may crash after its graph proposal commits but before the
/// establishment phase advances. Deriving IDs from the now-larger graph would
/// change the proposal under the same idempotency key. The immutable logical
/// operation therefore owns the ID namespace; physical retries reproduce the
/// exact same proposal regardless of observed graph state.
fn compile_continuation_task_graph(
    operation_id: &str,
    tasks: &[InitialWorkTask],
) -> Vec<InitialWorkItem> {
    let mut digest = Sha256::new();
    digest.update(b"continuation-work-items-v1\0");
    digest.update(operation_id.as_bytes());
    let namespace = format!("{:x}", digest.finalize());
    tasks
        .iter()
        .enumerate()
        .map(|(index, task)| InitialWorkItem {
            item_id: format!("task-{}-{}", &namespace[..48], index + 1),
            kind: "task",
            objective: task.objective.clone(),
            expected_result: task.expected_result.clone(),
        })
        .collect()
}

fn confirmed_assignment(
    executor: &RuntimeToolExecutor,
    run_id: &str,
    result: ToolResult,
) -> Result<Value, ToolResult> {
    if result.is_error {
        return Err(result);
    }
    let assignment: Value = serde_json::from_str(&result.output).map_err(|_| {
        ToolResult::error("Work assignment returned invalid structured lifecycle state".to_string())
    })?;
    if assignment.get("status").and_then(Value::as_str) != Some("assigned") {
        return Err(ToolResult::error(format!(
            "Work activation produced no assignment: {}",
            result.output
        )));
    }
    let Some(active) = executor.active_primary_work_attempt() else {
        return Err(ToolResult::error(
            "Work activation reported assignment without a durable active attempt".to_string(),
        ));
    };
    let matches = assignment.get("attempt_id").and_then(Value::as_str)
        == Some(active.attempt_id.as_str())
        && assignment.get("item_id").and_then(Value::as_str) == Some(active.item_id.as_str())
        && assignment.get("item_revision").and_then(Value::as_i64) == Some(active.item_revision)
        && active.executor_run_id == run_id;
    if !matches {
        return Err(ToolResult::error(
            "Work assignment receipt does not match the durable active attempt".to_string(),
        ));
    }
    Ok(assignment)
}

/// Apply the already-persisted semantic graph decision without asking the
/// primary model to author it again. The proposal identity and every new item
/// identity derive from the immutable establishment operation, so a crash
/// after proposal commit replays the same transition.
async fn apply_admitted_graph_mutations(
    executor: &RuntimeToolExecutor,
    establishment_request: &WorkEstablishmentRequest,
    decision: Option<&astra_services::WorkAdmissionDecision>,
    admitted_candidates: &[InitialWorkItem],
    invocation: ToolInvocationMetadata<'_>,
) -> Result<Option<AppliedGraphMutations>, ToolResult> {
    let mutations = decision.map_or(&[][..], |decision| decision.deferred_graph_mutations());
    if mutations.is_empty() {
        return Ok(None);
    }
    let Some(binding) = executor.work_binding.get() else {
        return Err(ToolResult::error(
            "canonical Work binding disappeared before graph mutations".to_string(),
        ));
    };
    let context = binding
        .repository
        .load_plan_context_for_session(&binding.owner_id, &binding.session_id)
        .await
        .map_err(|error| {
            ToolResult::error(format!(
                "canonical Work mutation context could not be read: {error}"
            ))
        })?;
    let addition_tasks = mutations
        .iter()
        .filter_map(astra_services::WorkAdmissionGraphMutation::addition)
        .map(|task| InitialWorkTask {
            objective: task.objective.clone(),
            expected_result: task.expected_result.clone(),
        })
        .collect::<Vec<_>>();
    let additions = compile_continuation_task_graph(
        &format!("{}-mutations", establishment_request.operation_id),
        &addition_tasks,
    );
    let mut affected_item_ids = additions
        .iter()
        .map(|item| item.item_id.clone())
        .collect::<Vec<_>>();
    let mut revisions = Vec::new();
    for mutation in mutations {
        let Some(candidate_number) = mutation.target_initial_candidate() else {
            continue;
        };
        let Some(candidate) = candidate_number
            .checked_sub(1)
            .and_then(|index| admitted_candidates.get(index))
        else {
            return Err(ToolResult::error(
                "persisted Work mutation targets an absent admitted candidate".to_string(),
            ));
        };
        let Some(retirement) = mutation.retirement() else {
            return Err(ToolResult::error(
                "persisted Work mutation has no typed retirement target".to_string(),
            ));
        };
        if retirement.objective != candidate.objective
            || retirement.expected_result != candidate.expected_result
        {
            return Err(ToolResult::error(
                "persisted Work mutation target conflicts with its admitted candidate".to_string(),
            ));
        }
        let Some(declaration_state) = mutation.required_declaration_state() else {
            return Err(ToolResult::error(
                "persisted Work mutation has no typed declaration transition".to_string(),
            ));
        };
        revisions.push(json!({
            "item_id": candidate.item_id,
            "expected_revision": WorkItemRevision::INITIAL.get(),
            "kind": candidate.kind,
            "objective": candidate.objective,
            "expected_result": candidate.expected_result,
            "declaration_state": declaration_state,
        }));
        affected_item_ids.push(candidate.item_id.clone());
    }
    let mutation_call_id = format!("{}-mutations", establishment_request.operation_id);
    let proposal_invocation = ToolInvocationMetadata {
        run_id: Some(establishment_request.operation_id.as_str()),
        turn_chain_id: Some(establishment_request.operation_id.as_str()),
        tool_call_id: Some(mutation_call_id.as_str()),
        ..invocation
    };
    let proposal = super::tool_work_plan::propose(
        executor,
        &json!({
            "context_id": context.context_id(),
            "reason": "Apply the persisted Work admission graph decision",
            "additions": additions,
            "revisions": revisions,
            "dependencies": [],
            "dependency_removals": [],
        }),
        proposal_invocation,
        None,
    )
    .await;
    if proposal.is_error {
        return Err(proposal);
    }
    let output: Value = serde_json::from_str(&proposal.output).map_err(|_| {
        ToolResult::error("canonical Work mutation returned invalid planning state".to_string())
    })?;
    if output.get("status").and_then(Value::as_str) != Some("accepted") {
        return Err(ToolResult::error(
            json!({
                "status": "establishment_pending",
                "phase": "graph_mutations",
                "proposal": output,
                "next_action": "resume_this_start_work_request_after_admission",
            })
            .to_string(),
        ));
    }
    let revision = output
        .get("result_graph_revision")
        .and_then(Value::as_i64)
        .filter(|revision| *revision > 0)
        .ok_or_else(|| {
            ToolResult::error(
                "canonical Work mutation returned no valid graph revision".to_string(),
            )
        })?;
    Ok(Some(AppliedGraphMutations {
        graph_revision: revision,
        affected_item_ids,
    }))
}

fn validate_initial_task_list(tasks: &[InitialWorkTask]) -> Result<(), &'static str> {
    for task in tasks {
        if WorkItemText::parse(task.objective.clone()).is_err()
            || WorkItemText::parse(task.expected_result.clone()).is_err()
        {
            return Err("start_work task list contains an invalid task");
        }
    }
    Ok(())
}

fn initial_runnable_items(items: &[InitialWorkItem]) -> Vec<InitialRunnableItem> {
    // The root receives exactly one initial foreground assignment. This is a
    // scheduling choice, not a declaration that later tasks depend on it.
    items
        .first()
        .map(|item| InitialRunnableItem {
            item_id: item.item_id.clone(),
            item_revision: WorkItemRevision::INITIAL.get(),
        })
        .into_iter()
        .collect()
}

fn initial_declared_tasks(items: &[InitialWorkItem]) -> Vec<InitialDeclaredTask> {
    items
        .iter()
        .map(|item| InitialDeclaredTask {
            item_id: item.item_id.clone(),
            item_revision: WorkItemRevision::INITIAL.get(),
        })
        .collect()
}

/// Reconcile a durable graph against the exact typed `start_work` payload.
///
/// Genesis and the first plan proposal are separate repository transactions,
/// so a crash can leave the root milestone plus only part of the requested
/// task list.  The recovery decision is structural: matching task identities
/// and text are retained, missing tasks are replayed, and any unexpected task
/// or conflicting immutable text fails closed.  No model-facing name or
/// substring heuristic participates in this decision.
fn initial_graph_recovery_items(
    snapshot: &astra_services::work::WorkTaskExecutionSnapshot,
    expected: &[InitialWorkItem],
) -> Result<Vec<InitialWorkItem>, &'static str> {
    let expected_by_id = expected
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect::<std::collections::HashMap<_, _>>();
    let mut present = HashSet::new();
    for item in snapshot.items() {
        if item.item_id.as_str() == "root" {
            continue;
        }
        let Some(expected_item) = expected_by_id.get(item.item_id.as_str()) else {
            return Err("durable Work graph contains an unexpected initial task");
        };
        if item.kind != astra_services::work::WorkItemKind::Task
            || item.revision != WorkItemRevision::INITIAL
            || item.declaration_state != astra_services::work::WorkItemDeclarationState::Active
            || item.objective.as_str() != expected_item.objective
            || item.expected_result.as_str() != expected_item.expected_result
        {
            return Err("durable Work graph conflicts with the exact initial task payload");
        }
        if !present.insert(item.item_id.as_str()) {
            return Err("durable Work graph contains a duplicate initial task identity");
        }
    }
    Ok(expected
        .iter()
        .filter(|item| !present.contains(item.item_id.as_str()))
        .cloned()
        .collect())
}

/// Stable identity for the durable Work-establishment operation.
///
/// This is intentionally distinct from the provider invocation id. A
/// transient retry must use a fresh tool-call identity so the invocation
/// ledger preserves its one-call/one-outcome invariant, while the same
/// canonical payload still addresses the same Work operation and can repair a
/// genesis whose initial graph proposal did not commit.
fn start_work_operation_id(
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    turn_chain_id: &str,
    args: &StartWorkArgs,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"start-work-tool-v1\0");
    digest.update(owner_id.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(session_id.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(turn_chain_id.as_bytes());
    digest.update(b"\0");
    digest.update(serde_json::to_vec(args).expect("typed start_work arguments must serialize"));
    format!("tool-{:x}", digest.finalize())
}

fn start_work_request_hash(
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    turn_chain_id: &str,
    args: &StartWorkArgs,
    admission_decision: Option<&astra_services::WorkAdmissionDecision>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"start-work-payload-v1\0");
    digest.update(owner_id.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(session_id.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(canonical_start_work_payload(turn_chain_id, args, admission_decision).as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

#[derive(Serialize)]
struct CanonicalStartWorkPayload<'a> {
    schema_version: u16,
    turn_chain_id: &'a str,
    arguments: &'a StartWorkArgs,
    #[serde(skip_serializing_if = "Option::is_none")]
    admission_decision: Option<&'a astra_services::WorkAdmissionDecision>,
}

fn canonical_start_work_payload(
    turn_chain_id: &str,
    args: &StartWorkArgs,
    admission_decision: Option<&astra_services::WorkAdmissionDecision>,
) -> String {
    serde_json::to_string(&CanonicalStartWorkPayload {
        schema_version: 2,
        turn_chain_id,
        arguments: args,
        admission_decision,
    })
    .expect("typed start_work payload must serialize")
}

pub(super) fn decode_canonical_work_establishment_payload(
    payload_json: &str,
) -> Result<(Value, Option<astra_services::WorkAdmissionDecision>), String> {
    let payload: Value = serde_json::from_str(payload_json)
        .map_err(|error| format!("invalid canonical Work payload: {error}"))?;
    if payload.get("schema_version").and_then(Value::as_u64) != Some(2) {
        return Err("unsupported canonical Work payload schema".to_string());
    }
    let arguments = payload
        .get("arguments")
        .cloned()
        .ok_or_else(|| "canonical Work payload has no start_work arguments".to_string())?;
    let decision = payload
        .get("admission_decision")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("invalid persisted Work admission decision: {error}"))?;
    Ok((arguments, decision))
}

/// Build the same immutable operation request before the synthetic carrier is
/// dispatched. Keeping this derivation at the lifecycle boundary lets the
/// host durably admit a semantic decision before a provider/tool phase runs,
/// while the handler reuses the exact IDs and payload on execution/recovery.
pub(super) fn canonical_work_establishment_request(
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    turn_chain_id: &str,
    run_id: &str,
    args_value: &Value,
    admission_decision: Option<&astra_services::WorkAdmissionDecision>,
) -> Result<WorkEstablishmentRequest, String> {
    let args: StartWorkArgs = serde_json::from_value(args_value.clone())
        .map_err(|error| format!("invalid canonical start_work payload: {error}"))?;
    if args.tasks.is_empty() || args.tasks.len() > 8 {
        return Err("canonical start_work task list exceeds its lifecycle bound".to_string());
    }
    validate_initial_task_list(&args.tasks).map_err(str::to_string)?;
    let request_id = start_work_operation_id(owner_id, session_id, turn_chain_id, &args);
    let creation = super::work_handlers::derive_work_creation(
        owner_id,
        WorkCreateRequestV1 {
            request_id,
            goal: args.goal.clone(),
            criteria: Vec::new(),
        },
    )
    .map_err(|_| "canonical start_work goal is invalid or too large".to_string())?;
    let activation = match args.activation {
        StartWorkActivation::Start => WorkEstablishmentActivation::Start,
        StartWorkActivation::Defer => WorkEstablishmentActivation::Defer,
    };
    Ok(WorkEstablishmentRequest {
        operation_id: start_work_operation_id(owner_id, session_id, turn_chain_id, &args),
        request_hash: start_work_request_hash(
            owner_id,
            session_id,
            turn_chain_id,
            &args,
            admission_decision,
        ),
        payload_json: canonical_start_work_payload(turn_chain_id, &args, admission_decision),
        owner_id: owner_id.clone(),
        work_id: creation.work_id,
        branch_id: creation.branch_id,
        session_id: session_id.clone(),
        run_id: run_id.to_string(),
        activation,
    })
}

fn successor_attempt_id(run_id: &str, tool_call_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"primary-work-successor-v1\0");
    digest.update(run_id.as_bytes());
    digest.update(b"\0");
    digest.update(tool_call_id.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Return the durable state of an already-bound Work instead of turning a
/// legitimate follow-up request into an opaque tool error.
///
/// A session owns one Work branch, but that branch is intentionally extensible:
/// a later user turn may add or revise items through the typed planning seam.
/// `start_work` is therefore idempotent at the runtime boundary.  It never
/// creates a second branch and never replays the genesis proposal.
async fn established_work_receipt(
    executor: &RuntimeToolExecutor,
    binding: &WorkRuntimeBinding,
    establishment_request: &WorkEstablishmentRequest,
    tasks: &[InitialWorkTask],
    activation: StartWorkActivation,
    requested_goal: &str,
    same_identity: bool,
) -> ToolResult {
    let snapshot = match binding
        .repository
        .load_task_execution_snapshot_for_session(&binding.owner_id, &binding.session_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return ToolResult::error(format!(
                "canonical Work is bound, but its durable state could not be read: {error}"
            ));
        }
    };
    let graph_state = match snapshot.next_foreground_task() {
        WorkTaskExecutionNext::Ready(_) => "ready",
        WorkTaskExecutionNext::InFlight(_) => "in_flight",
        WorkTaskExecutionNext::NeedsRecovery(_) => "needs_recovery",
        WorkTaskExecutionNext::Blocked => "blocked",
        WorkTaskExecutionNext::Complete => "complete",
    };
    let (status, next_action) = if same_identity {
        ("started", "inspect_existing_work_or_run_next_work_item")
    } else {
        ("continued", "inspect_existing_work_or_run_next_work_item")
    };
    let mut affected_item_ids = (!same_identity).then(|| {
        compile_continuation_task_graph(&establishment_request.operation_id, tasks)
            .into_iter()
            .map(|item| item.item_id)
            .collect::<HashSet<_>>()
    });
    if let (Some(ids), Some(active)) = (
        affected_item_ids.as_mut(),
        executor.active_primary_work_attempt(),
    ) {
        ids.insert(active.item_id);
    }
    let task_board_update = match task_board_update_from_snapshot(
        binding,
        &snapshot,
        same_identity.then_some(requested_goal),
        affected_item_ids.as_ref(),
    ) {
        Ok(update) => update,
        Err(error) => return error,
    };
    ToolResult::text(
        json!({
            "status": status,
            "replayed": true,
            "activation": activation,
            "work_id": binding.work_id,
            "branch_id": binding.branch_id,
            "graph_revision": snapshot.basis().graph_revision,
            "graph_state": graph_state,
            "item_count": snapshot.items().len(),
            "requested_goal": requested_goal,
            "task_board_update": task_board_update,
            "next_action": next_action,
        })
        .to_string(),
    )
}

/// Extend an existing session-bound Work branch in one typed server action.
///
/// The old path returned `already_bound` and forced the model to discover
/// `inspect_work_plan`, inspect a paginated graph, author ids, and then call
/// `propose_work_plan`. That was both expensive and error-prone for a normal
/// follow-up request. This path keeps the same proposal/admission authority,
/// but derives the current context and all new identities server-side.
async fn continue_bound_work(
    executor: &RuntimeToolExecutor,
    binding: &WorkRuntimeBinding,
    establishment: &DatabaseWorkEstablishmentService,
    establishment_request: &WorkEstablishmentRequest,
    requested_goal: &str,
    tasks: &[InitialWorkTask],
    admission_decision: Option<&astra_services::WorkAdmissionDecision>,
    activation: StartWorkActivation,
    proposal_invocation: ToolInvocationMetadata<'_>,
    assignment_invocation: ToolInvocationMetadata<'_>,
) -> ToolResult {
    let context = match binding
        .repository
        .load_plan_context_for_session(&binding.owner_id, &binding.session_id)
        .await
    {
        Ok(context) => context,
        Err(error) => {
            return ToolResult::error(format!(
                "canonical Work is bound, but its continuation context could not be read: {error}"
            ));
        }
    };
    if context.basis().work_id != binding.work_id || context.basis().branch_id != binding.branch_id
    {
        return ToolResult::error(
            "canonical Work continuation rejected because the session binding changed".to_string(),
        );
    }
    let additions = compile_continuation_task_graph(&establishment_request.operation_id, tasks);
    let declared_tasks = initial_declared_tasks(&additions);
    let proposal = super::tool_work_plan::propose(
        executor,
        &json!({
            "context_id": context.context_id(),
            // Keep the persisted change reason bounded and stable. The full
            // user goal is returned as a receipt field, not duplicated into a
            // repository audit string with a separate size contract.
            "reason": "Continue the canonical Work with the user's follow-up task list",
            "additions": additions,
            "revisions": [],
            "dependencies": [],
            "dependency_removals": []
        }),
        proposal_invocation,
        None,
    )
    .await;
    if proposal.is_error {
        return proposal;
    }
    let proposal_output: Value = match serde_json::from_str(&proposal.output) {
        Ok(value) => value,
        Err(_) => {
            return ToolResult::error(
                "canonical Work continuation returned invalid planning state".to_string(),
            );
        }
    };
    let status = proposal_output
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if status != "accepted" {
        return ToolResult::text(
            json!({
                "status": "continuation_pending",
                "activation": activation,
                "work_id": binding.work_id,
                "branch_id": binding.branch_id,
                "requested_goal": requested_goal,
                "declared_tasks": declared_tasks,
                "proposal": proposal_output,
                "next_action": "resume_this_start_work_request_after_admission",
            })
            .to_string(),
        );
    }
    let Some(mut graph_revision) = proposal_output
        .get("result_graph_revision")
        .and_then(Value::as_i64)
        .filter(|revision| *revision > 0)
    else {
        return ToolResult::error(
            "canonical Work continuation returned no valid graph revision".to_string(),
        );
    };
    if let Err(error) = establishment
        .advance_phase(
            establishment_request,
            WorkEstablishmentPhase::AwaitingPlan,
            WorkEstablishmentPhase::AwaitingAssignment,
        )
        .await
    {
        return ToolResult::error(format!(
            "canonical Work continuation committed its plan but could not record the assignment phase: {error}"
        ));
    }
    let mut affected_item_ids = additions
        .iter()
        .map(|item| item.item_id.clone())
        .collect::<HashSet<_>>();
    match apply_admitted_graph_mutations(
        executor,
        establishment_request,
        admission_decision,
        &additions,
        assignment_invocation,
    )
    .await
    {
        Ok(Some(applied)) => {
            graph_revision = applied.graph_revision;
            affected_item_ids.extend(applied.affected_item_ids);
        }
        Ok(None) => {}
        Err(result) => return result,
    }
    let (next_task, dispatch_error, next_action) = match activation {
        StartWorkActivation::Defer => (None, None::<String>, "await_explicit_continuation"),
        StartWorkActivation::Start => {
            let assignment =
                execute_run_next_work_item(executor, &json!({}), assignment_invocation).await;
            match confirmed_assignment(
                executor,
                assignment_invocation.run_id.unwrap_or_default(),
                assignment,
            ) {
                Ok(assignment) => (
                    Some(assignment),
                    None,
                    "execute_assigned_task_then_call_settle_work_item",
                ),
                Err(error) => return error,
            }
        }
    };
    if let Some(item_id) = next_task
        .as_ref()
        .and_then(|task| task.get("item_id"))
        .and_then(Value::as_str)
    {
        affected_item_ids.insert(item_id.to_string());
    }
    let board_update =
        match canonical_task_board_update(binding, None, Some(&affected_item_ids)).await {
            Ok(update) => update,
            Err(error) => return error,
        };
    ToolResult::text(
        json!({
            "status": "continued",
            "activation": activation,
            "work_id": binding.work_id,
            "branch_id": binding.branch_id,
            "requested_goal": requested_goal,
            "graph_revision": graph_revision,
            "declared_tasks": declared_tasks,
            "task_board_update": board_update,
            "next_task": next_task,
            "dispatch_error": dispatch_error,
            "next_action": next_action,
        })
        .to_string(),
    )
}

fn binding_matches_establishment_operation(
    binding: &WorkRuntimeBinding,
    request: &WorkEstablishmentRequest,
) -> bool {
    binding.owner_id == request.owner_id
        && binding.session_id == request.session_id
        && binding.work_id == request.work_id
        && binding.branch_id == request.branch_id
}

pub(super) async fn execute_start_work(
    executor: &RuntimeToolExecutor,
    args: &Value,
    invocation: ToolInvocationMetadata<'_>,
) -> ToolResult {
    let args: StartWorkArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(error) => return ToolResult::error(format!("Invalid start_work input: {error}")),
    };
    if args.tasks.is_empty() || args.tasks.len() > 8 {
        return ToolResult::error(
            "start_work task list exceeds the bounded lifecycle contract".to_string(),
        );
    }
    if let Err(message) = validate_initial_task_list(&args.tasks) {
        return ToolResult::error(message.to_string());
    }
    let (mut initial_items, initial_dependencies) = compile_initial_task_graph(&args.tasks);
    let admitted_initial_items = initial_items.clone();
    let activation = args.activation;
    let Some(run_id) = invocation.run_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return ToolResult::error(
            "start_work requires the exact current durable run identity".to_string(),
        );
    };
    let Some(_tool_call_id) = invocation
        .tool_call_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return ToolResult::error(
            "start_work requires the exact current tool-call identity".to_string(),
        );
    };
    let Some(turn_chain_id) = invocation
        .turn_chain_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return ToolResult::error(
            "start_work requires the exact current turn-chain identity".to_string(),
        );
    };
    let Some(pool) = executor.context_manifest_pool.clone() else {
        return ToolResult::error("canonical Work storage is unavailable".to_string());
    };
    let owner_id = match WorkOwnerId::parse(executor.user_id.clone()) {
        Ok(owner_id) => owner_id,
        Err(error) => return ToolResult::error(format!("invalid Work owner binding: {error}")),
    };
    let session_id = match InternalSessionId::parse(executor.session_id.clone()) {
        Ok(session_id) => session_id,
        Err(error) => return ToolResult::error(format!("invalid Work session binding: {error}")),
    };
    let work_goal = args.goal.clone();
    let request = WorkCreateRequestV1 {
        request_id: start_work_operation_id(&owner_id, &session_id, turn_chain_id, &args),
        goal: args.goal.clone(),
        // Model-authored Done-when remains provisional. Starting Work never
        // silently accepts criteria on the user's behalf.
        criteria: Vec::new(),
    };
    let mut creation = match super::work_handlers::derive_work_creation(&owner_id, request) {
        Ok(creation) => creation,
        Err(_) => return ToolResult::error("start_work goal is invalid or too large".to_string()),
    };
    let repository = astra_services::work::DatabaseWorkRepository::new(pool.clone());
    // Resolve an already durable session binding before admitting the
    // operation.  A follow-up `start_work` extends that branch; it must not
    // record a freshly derived Work identity and later pretend that identity
    // was established.  The read is only a target-selection hint: the
    // repository's genesis conflict/branch-binding checks remain authoritative.
    let attached_binding = executor.work_binding.get().cloned();
    let persisted_binding = if attached_binding.is_none() {
        match repository
            .load_session_plan_binding(&owner_id, &session_id)
            .await
        {
            Ok(binding) => Some(binding),
            Err(WorkRepositoryError::NotFound) => None,
            Err(error) => {
                return ToolResult::error(format!(
                    "start_work could not resolve the session Work binding: {error}"
                ));
            }
        }
    } else {
        None
    };
    if let Some(binding) = persisted_binding.as_ref() {
        if let Err(error) = executor.install_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            owner_id.clone(),
            session_id.clone(),
            binding.work_id.clone(),
            binding.branch_id.clone(),
        )) {
            return ToolResult::error(format!(
                "canonical Work exists but the current run could not bind its planning surface: {error}"
            ));
        }
    }
    let effective_binding = attached_binding.or_else(|| {
        persisted_binding.as_ref().map(|binding| {
            WorkRuntimeBinding::new(
                pool.clone(),
                owner_id.clone(),
                session_id.clone(),
                binding.work_id.clone(),
                binding.branch_id.clone(),
            )
        })
    });
    let mut requested_creation_identity = effective_binding.as_ref().is_none_or(|binding| {
        binding.work_id == creation.work_id && binding.branch_id == creation.branch_id
    });
    let operation_work_id = effective_binding
        .as_ref()
        .map(|binding| binding.work_id.clone())
        .unwrap_or_else(|| creation.work_id.clone());
    let operation_branch_id = effective_binding
        .as_ref()
        .map(|binding| binding.branch_id.clone())
        .unwrap_or_else(|| creation.branch_id.clone());
    let establishment = DatabaseWorkEstablishmentService::new(pool.clone());
    let trusted_invocation = invocation
        .tool_call_id
        .and_then(|call_id| executor.work_establishment_invocation(call_id));
    if let Some(WorkEstablishmentInvocation::DeferPending { operation_id }) =
        trusted_invocation.as_ref()
    {
        return defer_pending_work_establishment(
            executor,
            &establishment,
            &owner_id,
            &session_id,
            operation_id,
            run_id,
        )
        .await;
    }
    let trusted_operation_id =
        trusted_invocation
            .as_ref()
            .and_then(|invocation| match invocation {
                WorkEstablishmentInvocation::Establish { operation_id } => {
                    Some(operation_id.clone())
                }
                WorkEstablishmentInvocation::DeferPending { .. } => None,
            });
    let operation_id = trusted_operation_id
        .clone()
        .unwrap_or_else(|| start_work_operation_id(&owner_id, &session_id, turn_chain_id, &args));
    // A host-owned semantic admission may have committed a richer recovery
    // envelope before this physical tool invocation started. Reuse that exact
    // immutable request; rebuilding it from the current invocation would drop
    // the semantic decision and make a legitimate retry fail idempotency.
    let establishment_request = match establishment.load(&owner_id, &operation_id).await {
        Ok(operation) => WorkEstablishmentRequest {
            operation_id: operation.operation_id,
            request_hash: operation.request_hash,
            payload_json: operation.payload_json,
            owner_id: owner_id.clone(),
            work_id: operation.work_id,
            branch_id: operation.branch_id,
            session_id: operation.session_id,
            run_id: run_id.to_string(),
            activation: operation.activation,
        },
        Err(astra_services::work::WorkEstablishmentError::NotFound)
            if trusted_operation_id.is_none() =>
        {
            WorkEstablishmentRequest {
                operation_id,
                request_hash: start_work_request_hash(
                    &owner_id,
                    &session_id,
                    turn_chain_id,
                    &args,
                    None,
                ),
                payload_json: canonical_start_work_payload(turn_chain_id, &args, None),
                owner_id: owner_id.clone(),
                work_id: operation_work_id,
                branch_id: operation_branch_id,
                session_id: session_id.clone(),
                run_id: run_id.to_string(),
                activation: match activation {
                    StartWorkActivation::Start => WorkEstablishmentActivation::Start,
                    StartWorkActivation::Defer => WorkEstablishmentActivation::Defer,
                },
            }
        }
        Err(astra_services::work::WorkEstablishmentError::NotFound) => {
            return ToolResult::error(
                "trusted Work establishment provenance points to no durable operation".to_string(),
            );
        }
        Err(error) => {
            return ToolResult::error(format!(
                "start_work could not load its canonical operation: {error}"
            ));
        }
    };
    let (durable_args, admission_decision) =
        match decode_canonical_work_establishment_payload(&establishment_request.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                return ToolResult::error(format!(
                    "start_work canonical recovery envelope is invalid: {error}"
                ));
            }
        };
    let durable_args = match serde_json::from_value::<StartWorkArgs>(durable_args) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolResult::error(format!(
                "start_work canonical recovery arguments are invalid: {error}"
            ));
        }
    };
    if trusted_operation_id.is_some() && args != durable_args {
        return ToolResult::error(
            "trusted Work establishment arguments do not match its immutable durable payload"
                .to_string(),
        );
    }
    if trusted_operation_id.is_some() {
        creation = match super::work_handlers::derive_work_creation(
            &owner_id,
            WorkCreateRequestV1 {
                request_id: establishment_request.operation_id.clone(),
                goal: args.goal.clone(),
                criteria: Vec::new(),
            },
        ) {
            Ok(creation) => creation,
            Err(_) => {
                return ToolResult::error(
                    "durable Work establishment has invalid canonical creation identity"
                        .to_string(),
                );
            }
        };
        requested_creation_identity = effective_binding.as_ref().is_none_or(|binding| {
            binding.work_id == creation.work_id && binding.branch_id == creation.branch_id
        });
    }
    let mut operation = match establishment.admit(&establishment_request).await {
        Ok(operation) => operation,
        Err(error) => {
            return ToolResult::error(format!(
                "start_work could not durably admit its canonical operation: {error}"
            ));
        }
    };
    // A binding is not evidence that another Work won the race. Genesis is a
    // separate durable transaction, so a legitimate retry commonly observes
    // the binding created by this exact operation while its WAL is still in
    // AwaitingGenesis, AwaitingPlan, or AwaitingAssignment. The immutable
    // operation identity owns recovery; only a binding to a different
    // owner/session/Work/branch is a conflict.
    if let Some(binding) = executor.work_binding.get()
        && !binding_matches_establishment_operation(binding, &establishment_request)
    {
        return work_precondition_error(
            "work_establishment_binding_changed",
            "pending Work establishment conflicts with the canonical session binding",
            "refresh_work_state",
        );
    }
    if operation.is_complete() {
        let Some(binding) = executor.work_binding.get() else {
            return ToolResult::error(
                "Work establishment is complete but its session binding is unavailable".to_string(),
            );
        };
        if binding.work_id != establishment_request.work_id
            || binding.branch_id != establishment_request.branch_id
        {
            return ToolResult::error(
                "completed Work establishment points at a different session binding".to_string(),
            );
        }
        return established_work_receipt(
            executor,
            binding,
            &establishment_request,
            &args.tasks,
            activation,
            &work_goal,
            requested_creation_identity,
        )
        .await;
    }
    if operation.is_aborted() {
        return work_precondition_error(
            "work_establishment_terminal",
            format!(
                "this Work establishment is already terminal ({:?}); start a new user turn to establish or continue Work",
                operation.state
            ),
            "start_new_user_turn",
        );
    }
    let mut planning_is_durable = false;
    let mut recovered_graph_revision = None;
    if let Some(binding) = executor.work_binding.get() {
        let same_identity =
            binding.work_id == creation.work_id && binding.branch_id == creation.branch_id;
        if !requested_creation_identity {
            if operation.phase == WorkEstablishmentPhase::AwaitingGenesis {
                if let Err(error) = establishment
                    .advance_phase(
                        &establishment_request,
                        WorkEstablishmentPhase::AwaitingGenesis,
                        WorkEstablishmentPhase::AwaitingPlan,
                    )
                    .await
                {
                    return ToolResult::error(format!(
                        "start_work could not durably record its continuation phase: {error}"
                    ));
                }
            }
            let proposal_invocation = ToolInvocationMetadata {
                run_id: Some(establishment_request.operation_id.as_str()),
                turn_chain_id: Some(establishment_request.operation_id.as_str()),
                tool_call_id: Some(establishment_request.operation_id.as_str()),
                ..invocation
            };
            let result = continue_bound_work(
                executor,
                binding,
                &establishment,
                &establishment_request,
                &work_goal,
                &args.tasks,
                admission_decision.as_ref(),
                activation,
                proposal_invocation,
                invocation,
            )
            .await;
            if result.is_error {
                let _ = establishment
                    .record_error(&establishment_request, &result.output)
                    .await;
                return result;
            }
            let continuation_output: Value = match serde_json::from_str(&result.output) {
                Ok(output) => output,
                Err(_) => {
                    let result = ToolResult::error(
                        "continuation returned invalid structured lifecycle state".to_string(),
                    );
                    let _ = establishment
                        .record_error(&establishment_request, &result.output)
                        .await;
                    return result;
                }
            };
            if continuation_output.get("status").and_then(Value::as_str)
                == Some("continuation_pending")
            {
                // Policy admission is a durable phase of this operation, not
                // a successful establishment. Keep AwaitingPlan so the same
                // canonical payload can resume after the proposal is resolved.
                return result;
            }
            if continuation_output
                .get("dispatch_error")
                .is_some_and(|error| !error.is_null())
            {
                let _ = establishment
                    .record_error(&establishment_request, &result.output)
                    .await;
                return result;
            }
            if continuation_output.get("status").and_then(Value::as_str) != Some("continued") {
                let result = ToolResult::error(
                    "continuation returned an unknown lifecycle status".to_string(),
                );
                let _ = establishment
                    .record_error(&establishment_request, &result.output)
                    .await;
                return result;
            }
            // Complete describes establishment of the admitted activation,
            // not completion of any WorkItem. `Start` reached this seam only
            // after exact assignment confirmation; `Defer` deliberately did
            // not create an assignment.
            let terminal = establishment
                .advance_phase(
                    &establishment_request,
                    WorkEstablishmentPhase::AwaitingAssignment,
                    WorkEstablishmentPhase::Complete,
                )
                .await;
            if let Err(error) = terminal {
                return ToolResult::error(format!(
                    "continuation completed but its durable receipt could not be committed: {error}"
                ));
            }
            return result;
        }
        if same_identity {
            // Genesis and the initial graph proposal are one user-visible
            // operation, but they are persisted by separate repository
            // transactions today.  A failure after genesis can therefore
            // leave a durable Work branch with an empty graph.  Do not turn
            // that incomplete state into a successful `already_started`
            // receipt: the exact retry payload is the recovery authority and
            // must be allowed to finish the initial proposal below.  Once an
            // item exists, the operation has crossed its durable planning
            // boundary and the normal idempotent receipt is correct.
            let snapshot = match binding
                .repository
                .load_task_execution_snapshot_for_session(&binding.owner_id, &binding.session_id)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return ToolResult::error(format!(
                        "canonical Work is bound, but its initial planning state could not be read: {error}"
                    ));
                }
            };
            if operation.phase == WorkEstablishmentPhase::AwaitingGenesis {
                operation = match establishment
                    .advance_phase(
                        &establishment_request,
                        WorkEstablishmentPhase::AwaitingGenesis,
                        WorkEstablishmentPhase::AwaitingPlan,
                    )
                    .await
                {
                    Ok(operation) => operation,
                    Err(error) => {
                        return ToolResult::error(format!(
                            "Work exists but its durable genesis phase could not be recorded: {error}"
                        ));
                    }
                };
            }
            if operation.phase >= WorkEstablishmentPhase::AwaitingAssignment {
                // The durable phase is the operation WAL. At this point the
                // initial proposal was accepted and later deterministic
                // mutations may legitimately have retired candidates or
                // added operation-owned items, so re-validating the graph as
                // a pristine initial shape would reject successful recovery.
                recovered_graph_revision = Some(snapshot.basis().graph_revision.get());
                planning_is_durable = true;
            } else {
                match initial_graph_recovery_items(&snapshot, &initial_items) {
                    Ok(missing) if missing.is_empty() => {
                        recovered_graph_revision = Some(snapshot.basis().graph_revision.get());
                        if operation.phase == WorkEstablishmentPhase::AwaitingPlan {
                            if let Err(error) = establishment
                                .advance_phase(
                                    &establishment_request,
                                    WorkEstablishmentPhase::AwaitingPlan,
                                    WorkEstablishmentPhase::AwaitingAssignment,
                                )
                                .await
                            {
                                return ToolResult::error(format!(
                                    "Work initial planning is present but its durable phase could not be recorded: {error}"
                                ));
                            }
                        }
                        planning_is_durable = true;
                    }
                    Ok(missing) => {
                        if operation.phase >= WorkEstablishmentPhase::AwaitingAssignment {
                            return ToolResult::error(
                            "durable Work establishment phase claims a complete initial graph, but the graph is partial"
                                .to_string(),
                        );
                        }
                        initial_items = missing;
                    }
                    Err(error) => return ToolResult::error(error.to_string()),
                }
            }
            // Keep the existing binding and retry only the missing portion of
            // the exact initial planning seam. The proposal identity remains
            // derived from trusted invocation metadata, so a fresh provider
            // call can repair a partial graph without minting a second Work.
        }
    } else {
        let create_result = repository
            .create_genesis_in_running_session(
                creation.genesis.in_session(session_id.clone()),
                run_id,
            )
            .await;
        match create_result {
            Ok(_) => {}
            Err(WorkRepositoryError::Conflict { .. }) => {
                match repository
                    .load_session_plan_binding(&owner_id, &session_id)
                    .await
                {
                    Ok(existing) => {
                        let same_identity = existing.work_id == creation.work_id
                            && existing.branch_id == creation.branch_id;
                        if let Err(error) = executor.install_work_binding(WorkRuntimeBinding::new(
                            pool.clone(),
                            owner_id.clone(),
                            session_id.clone(),
                            existing.work_id.clone(),
                            existing.branch_id.clone(),
                        )) {
                            return ToolResult::error(format!(
                                "canonical Work was found but the current run could not bind its planning surface: {error}"
                            ));
                        }
                        if !same_identity {
                            let result = ToolResult::error(
                                "start_work observed a concurrent session binding for a different Work identity; retry from the canonical session state"
                                    .to_string(),
                            );
                            let _ = establishment
                                .record_error(&establishment_request, &result.output)
                                .await;
                            return result;
                        }
                        let snapshot = match repository
                            .load_task_execution_snapshot_for_session(&owner_id, &session_id)
                            .await
                        {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                return ToolResult::error(format!(
                                    "start_work retry could not read canonical planning state: {error}"
                                ));
                            }
                        };
                        if operation.phase == WorkEstablishmentPhase::AwaitingGenesis {
                            operation = match establishment
                                .advance_phase(
                                    &establishment_request,
                                    WorkEstablishmentPhase::AwaitingGenesis,
                                    WorkEstablishmentPhase::AwaitingPlan,
                                )
                                .await
                            {
                                Ok(operation) => operation,
                                Err(error) => {
                                    return ToolResult::error(format!(
                                        "start_work retry could not record its durable genesis phase: {error}"
                                    ));
                                }
                            };
                        }
                        if operation.phase >= WorkEstablishmentPhase::AwaitingAssignment {
                            recovered_graph_revision = Some(snapshot.basis().graph_revision.get());
                            planning_is_durable = true;
                        } else {
                            match initial_graph_recovery_items(&snapshot, &initial_items) {
                                Ok(missing) if missing.is_empty() => {
                                    recovered_graph_revision =
                                        Some(snapshot.basis().graph_revision.get());
                                    if operation.phase == WorkEstablishmentPhase::AwaitingPlan {
                                        if let Err(error) = establishment
                                            .advance_phase(
                                                &establishment_request,
                                                WorkEstablishmentPhase::AwaitingPlan,
                                                WorkEstablishmentPhase::AwaitingAssignment,
                                            )
                                            .await
                                        {
                                            return ToolResult::error(format!(
                                                "start_work retry could not record its durable planning phase: {error}"
                                            ));
                                        }
                                    }
                                    planning_is_durable = true;
                                }
                                Ok(missing) => initial_items = missing,
                                Err(error) => return ToolResult::error(error.to_string()),
                            }
                        }
                    }
                    Err(error) => {
                        return ToolResult::error(format!(
                            "start_work retry could not confirm canonical state: {error}"
                        ));
                    }
                }
            }
            Err(WorkRepositoryError::SessionBusy) => {
                return ToolResult::error(
                    "start_work was rejected because this run no longer owns the conversation execution slot"
                        .to_string(),
                );
            }
            Err(error) => return ToolResult::error(format!("start_work failed: {error}")),
        }
        if let Err(error) = executor.install_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            owner_id.clone(),
            session_id.clone(),
            creation.work_id.clone(),
            creation.branch_id.clone(),
        )) {
            return ToolResult::error(format!(
                "Work was created but the current run could not bind its planning surface: {error}"
            ));
        }
        if let Err(error) = establishment
            .advance_phase(
                &establishment_request,
                WorkEstablishmentPhase::AwaitingGenesis,
                WorkEstablishmentPhase::AwaitingPlan,
            )
            .await
        {
            return ToolResult::error(format!(
                "Work genesis committed but its durable phase could not be recorded: {error}"
            ));
        }
    }

    // Establishment and initial decomposition are one model-facing action.
    // Reuse the exact Work proposal seam so graph validation, owner isolation,
    // optimistic concurrency, admission, and retries have one implementation.
    let mut graph_revision = recovered_graph_revision.unwrap_or_default();
    if !planning_is_durable {
        let inspected = super::tool_work_plan::inspect(executor, &json!({}), None).await;
        if inspected.is_error {
            let _ = establishment
                .record_error(&establishment_request, &inspected.output)
                .await;
            return inspected;
        }
        let inspected: Value = match serde_json::from_str(&inspected.output) {
            Ok(value) => value,
            Err(_) => {
                let result = ToolResult::error(
                    "Work was created but its initial planning context was invalid".to_string(),
                );
                let _ = establishment
                    .record_error(&establishment_request, &result.output)
                    .await;
                return result;
            }
        };
        let Some(context_id) = inspected.get("context_id").and_then(Value::as_str) else {
            let result = ToolResult::error(
                "Work was created but its initial planning context had no identity".to_string(),
            );
            let _ = establishment
                .record_error(&establishment_request, &result.output)
                .await;
            return result;
        };
        let proposal_invocation = ToolInvocationMetadata {
            run_id: Some(establishment_request.operation_id.as_str()),
            turn_chain_id: Some(establishment_request.operation_id.as_str()),
            tool_call_id: Some(establishment_request.operation_id.as_str()),
            ..invocation
        };
        let proposal = super::tool_work_plan::propose(
            executor,
            &json!({
                "context_id": context_id,
                "reason": "Initial decomposition of the user's durable goal",
                "additions": initial_items,
                "revisions": [],
                "dependencies": initial_dependencies,
                "dependency_removals": []
            }),
            proposal_invocation,
            None,
        )
        .await;
        if proposal.is_error {
            let _ = establishment
                .record_error(&establishment_request, &proposal.output)
                .await;
            return proposal;
        }
        let proposal_output: Value = serde_json::from_str(&proposal.output).unwrap_or(Value::Null);
        let Some(result_graph_revision) = proposal_output
            .get("result_graph_revision")
            .and_then(Value::as_u64)
            .filter(|revision| *revision > 0 && *revision <= i64::MAX as u64)
            .map(|revision| revision as i64)
        else {
            let result = ToolResult::error(
                "Work initial planning returned no valid graph revision".to_string(),
            );
            let _ = establishment
                .record_error(&establishment_request, &result.output)
                .await;
            return result;
        };
        graph_revision = result_graph_revision;
        if let Err(error) = establishment
            .advance_phase(
                &establishment_request,
                WorkEstablishmentPhase::AwaitingPlan,
                WorkEstablishmentPhase::AwaitingAssignment,
            )
            .await
        {
            return ToolResult::error(format!(
                "Work initial planning committed but its durable phase could not be recorded: {error}"
            ));
        }
    }
    match apply_admitted_graph_mutations(
        executor,
        &establishment_request,
        admission_decision.as_ref(),
        &admitted_initial_items,
        invocation,
    )
    .await
    {
        Ok(Some(applied)) => graph_revision = applied.graph_revision,
        Ok(None) => {}
        Err(result) => {
            let _ = establishment
                .record_error(&establishment_request, &result.output)
                .await;
            return result;
        }
    }
    let initial_item_count = initial_items.len();
    let runnable_items = initial_runnable_items(&initial_items);
    let declared_tasks = initial_declared_tasks(&initial_items);
    // Ordinary Work establishment and its first executable assignment are
    // one product action. Explicitly deferred Work remains durably Ready and
    // owns no attempt until a later typed run_next_work_item call.
    let (initial_task, dispatch_error, next_action) = match activation {
        StartWorkActivation::Defer => (None, None::<String>, "await_explicit_continuation"),
        StartWorkActivation::Start => {
            let initial_assignment =
                execute_run_next_work_item(executor, &json!({}), invocation).await;
            match confirmed_assignment(executor, run_id, initial_assignment) {
                Ok(assignment) => (
                    Some(assignment),
                    None,
                    "execute_initial_task_then_call_settle_work_item",
                ),
                Err(result) => {
                    let _ = establishment
                        .record_error(&establishment_request, &result.output)
                        .await;
                    return result;
                }
            }
        }
    };
    if let Some(error) = dispatch_error.as_deref() {
        let _ = establishment
            .record_error(&establishment_request, error)
            .await;
    } else {
        // Complete describes establishment of the admitted activation, not
        // completion of a WorkItem. The branch above enforces the activation-
        // specific assignment postcondition before this shared transition.
        let terminal = establishment
            .advance_phase(
                &establishment_request,
                WorkEstablishmentPhase::AwaitingAssignment,
                WorkEstablishmentPhase::Complete,
            )
            .await;
        if let Err(error) = terminal {
            return ToolResult::error(format!(
                "Work establishment finished but its durable terminal receipt could not be committed: {error}"
            ));
        }
    }
    let Some(binding) = executor.work_binding.get() else {
        return ToolResult::error(
            "Work establishment committed without installing its canonical binding".to_string(),
        );
    };
    let task_board_update = match canonical_task_board_update(binding, Some(&work_goal), None).await
    {
        Ok(update) => update,
        Err(error) => return error,
    };
    ToolResult::text(
        json!({
            "status": "started",
            "activation": activation,
            "work_id": establishment_request.work_id,
            "branch_id": establishment_request.branch_id,
            "graph_revision": graph_revision,
            "initial_item_count": initial_item_count,
            "declared_tasks": declared_tasks,
            "runnable_items": runnable_items,
            "task_board_update": task_board_update,
            "initial_task": initial_task,
            "dispatch_error": dispatch_error,
            "next_action": next_action,
        })
        .to_string(),
    )
}

/// Read the next task from the durable execution snapshot. The coordinator
/// receives no task identity or free-form worker instruction from the model:
/// Work owns sequencing, immutable identity, and the worker brief.
async fn load_next_active_task(
    executor: &RuntimeToolExecutor,
) -> Result<(astra_services::work::GraphRevision, WorkTaskExecutionNext), astra_tools::ToolResult> {
    let Some(binding) = executor.work_binding.get() else {
        return Err(work_precondition_error(
            WORK_ERROR_KIND_NOT_BOUND,
            "run_next_work_item requires a canonical Work bound to this session",
            "start_work",
        ));
    };
    if let Some(pool) = executor.context_manifest_pool.clone() {
        if let Err(error) = astra_services::work::DatabaseWorkAttemptSettlementService::new(pool)
            .reconcile_terminal_primary_attempts(
                binding.owner_id.as_str(),
                binding.work_id.as_str(),
                binding.branch_id.as_str(),
            )
            .await
        {
            return Err(ToolResult::error(format!(
                "run_next_work_item could not reconcile prior attempts: {error}"
            )));
        }
    }
    let snapshot = binding
        .repository
        .load_task_execution_snapshot_for_session(&binding.owner_id, &binding.session_id)
        .await
        .map_err(|error| {
            ToolResult::error(format!("run_next_work_item could not load Work: {error}"))
        })?;
    if snapshot.basis().work_id != binding.work_id
        || snapshot.basis().branch_id != binding.branch_id
    {
        return Err(work_precondition_error(
            WORK_ERROR_KIND_BINDING_CHANGED,
            "run_next_work_item rejected because the session Work binding changed",
            "inspect_work_plan",
        ));
    }
    Ok((
        snapshot.basis().graph_revision,
        snapshot.next_foreground_task(),
    ))
}

#[derive(Clone, Debug)]
struct RestoredPrimaryWorkAttempt {
    active: ActivePrimaryWorkAttempt,
    task_board_update: WorkTaskBoardUpdateV1,
}

/// Rebind the one durable in-flight foreground task to this run.
///
/// Active-attempt identity is durable session state, not model context. A
/// continuation must therefore restore it before the first provider boundary
/// instead of hoping the model rediscovers `run_next_work_item`. The database
/// transition is the ownership authority and rejects cross-session or
/// still-live-run takeover.
async fn restore_primary_attempt_from_selection(
    executor: &RuntimeToolExecutor,
    run_id: &str,
    selected: &WorkTaskExecutionNext,
) -> Result<Option<RestoredPrimaryWorkAttempt>, String> {
    let WorkTaskExecutionNext::InFlight(item) = selected else {
        return Ok(None);
    };
    let Some(attempt) = item.execution.run.as_ref() else {
        return Ok(None);
    };

    if attempt.run_id != run_id {
        if item.execution.status != WorkItemExecutionStatus::Paused {
            return Ok(None);
        }
        let pool = executor
            .context_manifest_pool
            .clone()
            .ok_or_else(|| "canonical Work storage is unavailable".to_string())?;
        let taken_over = astra_services::work::DatabaseWorkAttemptSettlementService::new(pool)
            .take_over_paused_primary_attempt(
                &executor.user_id,
                attempt.attempt_id.as_str(),
                run_id,
            )
            .await
            .map_err(|error| format!("could not take over paused Work task: {error}"))?;
        if !taken_over {
            return Ok(None);
        }
    }

    let active = ActivePrimaryWorkAttempt {
        attempt_id: attempt.attempt_id.as_str().to_string(),
        executor_run_id: run_id.to_string(),
        item_id: item.item_id.as_str().to_string(),
        item_revision: item.revision.get(),
        objective: item.objective.as_str().to_string(),
        expected_result: item.expected_result.as_str().to_string(),
    };
    executor
        .install_active_primary_work_attempt(active.clone())
        .map_err(|error| format!("could not install restored Work task: {error}"))?;
    let binding = executor
        .work_binding
        .get()
        .ok_or_else(|| "canonical Work binding disappeared".to_string())?;
    let task_board_update = board_update(
        binding.work_id.as_str().to_string(),
        binding.branch_id.as_str().to_string(),
        None,
        vec![board_task_for_active_attempt(
            &active,
            WorkTaskBoardExecutionStatusV1::Running,
            WorkTaskBoardDeliveryStatusV1::Unreported,
            None,
            None,
            Vec::new(),
        )],
    );
    Ok(Some(RestoredPrimaryWorkAttempt {
        active,
        task_board_update,
    }))
}

/// Restore a continuation's paused foreground task before any model call and
/// return the typed board projection that makes the ownership change visible.
pub(super) async fn restore_primary_work_attempt_for_run(
    executor: &RuntimeToolExecutor,
    run_id: &str,
) -> Result<Option<Value>, String> {
    if !executor.has_work_binding() || executor.has_active_primary_work_attempt() {
        return Ok(None);
    }
    let (_, selected) = load_next_active_task(executor)
        .await
        .map_err(|error| error.output)?;
    let Some(restored) =
        restore_primary_attempt_from_selection(executor, run_id, &selected).await?
    else {
        return Ok(None);
    };
    Ok(Some(json!({
        "type": "work_task_board_update",
        "session_id": executor.session_id,
        "task_board_update": restored.task_board_update,
    })))
}

/// Atomically bind one dependency-ready task to the primary session. The
/// model supplies no task identity or execution carrier; Work owns selection.
pub(super) async fn execute_run_next_work_item(
    executor: &RuntimeToolExecutor,
    args: &Value,
    invocation: ToolInvocationMetadata<'_>,
) -> ToolResult {
    if !args.as_object().is_some_and(serde_json::Map::is_empty) {
        return ToolResult::error("run_next_work_item accepts no arguments".to_string());
    }
    let Some(run_id) = invocation.run_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return ToolResult::error(
            "run_next_work_item requires the exact current durable run identity".to_string(),
        );
    };
    let Some(tool_call_id) = invocation
        .tool_call_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return ToolResult::error(
            "run_next_work_item requires the exact current tool-call identity".to_string(),
        );
    };
    if let Some(active) = executor.active_primary_work_attempt() {
        return ToolResult::text(
            json!({
                "status": "assigned",
                "item_id": active.item_id,
                "item_revision": active.item_revision,
                "attempt_id": active.attempt_id,
                "objective": active.objective,
                "expected_result": active.expected_result,
                "completion_rule": "settle_immediately_when_expected_result_is_satisfied_without_broadening_scope",
                "execution": "primary_session_resumed",
                "next_action": "resume_this_task_then_call_settle_work_item"
            })
            .to_string(),
        );
    }
    let (graph_revision, selected) = match load_next_active_task(executor).await {
        Ok(selection) => selection,
        Err(error) => return error,
    };
    match restore_primary_attempt_from_selection(executor, run_id, &selected).await {
        Ok(Some(restored)) => {
            let active = restored.active;
            return ToolResult::text(
                json!({
                    "status": "assigned",
                    "item_id": active.item_id,
                    "item_revision": active.item_revision,
                    "attempt_id": active.attempt_id,
                    "objective": active.objective,
                    "expected_result": active.expected_result,
                    "completion_rule": "settle_immediately_when_expected_result_is_satisfied_without_broadening_scope",
                    "execution": "primary_session_resumed",
                    "task_board_update": restored.task_board_update,
                    "next_action": "resume_this_task_then_call_settle_work_item"
                })
                .to_string(),
            );
        }
        Ok(None) => {}
        Err(error) => {
            return ToolResult::error(format!(
                "run_next_work_item could not restore its active task: {error}"
            ));
        }
    }
    let WorkTaskExecutionNext::Ready(item) = selected else {
        let (status, item_id) = match selected {
            WorkTaskExecutionNext::InFlight(item) => ("in_flight", Some(item.item_id)),
            WorkTaskExecutionNext::NeedsRecovery(item) => ("needs_recovery", Some(item.item_id)),
            WorkTaskExecutionNext::Blocked => ("blocked", None),
            WorkTaskExecutionNext::Complete => ("complete", None),
            WorkTaskExecutionNext::Ready(_) => unreachable!("handled above"),
        };
        return ToolResult::text(
            json!({
                "status": status,
                "item_id": item_id.map(|id| id.as_str().to_string()),
                "next_action": "inspect_or_update_canonical_work_before_another_execution_attempt"
            })
            .to_string(),
        );
    };
    let Some(binding) = executor.work_binding.get() else {
        return ToolResult::error("canonical Work binding disappeared".to_string());
    };
    let mut digest = Sha256::new();
    digest.update(b"primary-work-attempt-v1\0");
    digest.update(run_id.as_bytes());
    digest.update(b"\0");
    digest.update(tool_call_id.as_bytes());
    let attempt_id = format!("{:x}", digest.finalize());
    let attempt = match WorkItemAttemptId::parse(attempt_id.clone()) {
        Ok(attempt) => attempt,
        Err(error) => return ToolResult::error(format!("invalid Work attempt identity: {error}")),
    };
    let Some(pool) = executor.context_manifest_pool.clone() else {
        return ToolResult::error("canonical Work storage is unavailable".to_string());
    };
    let service = astra_services::work::DatabaseWorkAttemptSettlementService::new(pool);
    if let Err(error) = service
        .begin_attempt(NewWorkItemAttempt {
            owner_id: binding.owner_id.clone(),
            work_id: binding.work_id.clone(),
            branch_id: binding.branch_id.clone(),
            session_id: binding.session_id.as_str().to_string(),
            item: WorkItemRevisionRef {
                item_id: item.item_id.clone(),
                revision: item.revision,
            },
            graph_revision,
            attempt_id: attempt,
            executor_run_id: run_id.to_string(),
            execution_mode: WorkAttemptExecutionMode::Primary,
        })
        .await
    {
        return ToolResult::error(format!("run_next_work_item rejected: {error}"));
    }
    let active_attempt = ActivePrimaryWorkAttempt {
        attempt_id: attempt_id.clone(),
        executor_run_id: run_id.to_string(),
        item_id: item.item_id.as_str().to_string(),
        item_revision: item.revision.get(),
        objective: item.objective.as_str().to_string(),
        expected_result: item.expected_result.as_str().to_string(),
    };
    if let Err(error) = executor.install_active_primary_work_attempt(active_attempt.clone()) {
        return ToolResult::error(format!(
            "Work task was admitted but could not be activated: {error}"
        ));
    }
    let task_board_update = board_update(
        binding.work_id.as_str().to_string(),
        binding.branch_id.as_str().to_string(),
        Some(graph_revision.get()),
        vec![board_task_for_active_attempt(
            &active_attempt,
            WorkTaskBoardExecutionStatusV1::Running,
            WorkTaskBoardDeliveryStatusV1::Unreported,
            None,
            None,
            Vec::new(),
        )],
    );
    ToolResult::text(
        json!({
            "status": "assigned",
            "item_id": item.item_id,
            "item_revision": item.revision,
            "attempt_id": attempt_id,
            "objective": item.objective,
            "expected_result": item.expected_result,
            "completion_rule": "settle_immediately_when_expected_result_is_satisfied_without_broadening_scope",
            "execution": "primary_session",
            "task_board_update": task_board_update,
            "next_action": "execute_this_task_directly_then_call_settle_work_item"
        })
        .to_string(),
    )
}

pub(super) async fn execute_settle_work_item(
    executor: &RuntimeToolExecutor,
    args: &Value,
    invocation: ToolInvocationMetadata<'_>,
) -> ToolResult {
    let Some(run_id) = invocation.run_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return ToolResult::error(
            "settle_work_item requires the exact current durable run identity".to_string(),
        );
    };
    let settlement: astra_services::work::NewWorkAttemptSettlement =
        match serde_json::from_value(args.clone()) {
            Ok(settlement) => settlement,
            Err(error) => {
                return ToolResult::error(format!("Invalid settle_work_item input: {error}"));
            }
        };
    let Some(pool) = executor.context_manifest_pool.clone() else {
        return ToolResult::error("canonical Work storage is unavailable".to_string());
    };
    let active = executor.active_primary_work_attempt();
    if active
        .as_ref()
        .is_some_and(|active| active.executor_run_id != run_id)
    {
        return ToolResult::error(
            "settle_work_item run does not own the active primary Work task".to_string(),
        );
    }
    let service = astra_services::work::DatabaseWorkAttemptSettlementService::new(pool);
    let recorded = match active.as_ref() {
        Some(active) => {
            let Some(expected_control_epoch) = invocation.expected_control_epoch else {
                return ToolResult::error(
                    "primary settle_work_item requires durable action-admission authority"
                        .to_string(),
                );
            };
            let Some(tool_call_id) = invocation
                .tool_call_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                return ToolResult::error(
                    "settle_work_item requires the exact current tool-call identity".to_string(),
                );
            };
            let successor_attempt_id =
                match WorkItemAttemptId::parse(successor_attempt_id(run_id, tool_call_id)) {
                    Ok(attempt_id) => attempt_id,
                    Err(error) => {
                        return ToolResult::error(format!(
                            "invalid successor Work attempt identity: {error}"
                        ));
                    }
                };
            service
                .record_and_advance_primary(
                    &executor.user_id,
                    &active.attempt_id,
                    run_id,
                    expected_control_epoch,
                    settlement,
                    successor_attempt_id,
                )
                .await
                .map(PrimarySettlementResult::Advanced)
        }
        // Explicitly delegated children carry immutable WorkItem identity on
        // their durable Run; no prompt text or model-supplied ID participates.
        None => service
            .record_for_run(&executor.user_id, run_id, settlement)
            .await
            .map(PrimarySettlementResult::Recorded),
    };
    match recorded {
        Ok(PrimarySettlementResult::Advanced(advanced)) => {
            let recorded = advanced.settlement;
            let execution_status = task_graph_execution_status(&advanced.advance);
            // This is deliberately the execution state of the declared task
            // graph, not the delivery/acceptance state of the Work. A Work
            // can have no runnable tasks while review criteria remain
            // unaccepted or verification evidence is still pending.
            let (successor, next_task, next_action) = match advanced.advance {
                astra_services::work::PrimaryWorkAttemptAdvance::Assigned {
                    attempt_id,
                    item_id,
                    item_revision,
                    objective,
                    expected_result,
                    resumed,
                } => {
                    let successor = ActivePrimaryWorkAttempt {
                        attempt_id: attempt_id.as_str().to_string(),
                        executor_run_id: run_id.to_string(),
                        item_id: item_id.as_str().to_string(),
                        item_revision: item_revision.get(),
                        objective: objective.as_str().to_string(),
                        expected_result: expected_result.as_str().to_string(),
                    };
                    let next_task = json!({
                        "status": "assigned",
                        "item_id": item_id,
                        "item_revision": item_revision,
                        "attempt_id": attempt_id,
                        "objective": objective,
                        "expected_result": expected_result,
                        "completion_rule": "settle_immediately_when_expected_result_is_satisfied_without_broadening_scope",
                        "execution": if resumed { "primary_session_resumed" } else { "primary_session" }
                    });
                    (
                        Some(successor),
                        Some(next_task),
                        "execute_next_task_then_call_settle_work_item",
                    )
                }
                astra_services::work::PrimaryWorkAttemptAdvance::NeedsRecovery => {
                    (None, None, "inspect_or_update_canonical_work")
                }
                astra_services::work::PrimaryWorkAttemptAdvance::Blocked => {
                    (None, None, "inspect_or_update_canonical_work")
                }
                astra_services::work::PrimaryWorkAttemptAdvance::Complete => {
                    (None, None, "synthesize_final_response")
                }
            };
            let Some(active) = active else {
                return ToolResult::error("primary Work attempt state disappeared".to_string());
            };
            let settled_task = board_settled_task(&active, &recorded);
            let settlement_transition = canonical_settlement_transition(&settled_task);
            let mut changed_tasks = vec![settled_task];
            if let Some(successor) = successor.as_ref() {
                changed_tasks.push(board_task_for_active_attempt(
                    successor,
                    WorkTaskBoardExecutionStatusV1::Running,
                    WorkTaskBoardDeliveryStatusV1::Unreported,
                    None,
                    None,
                    Vec::new(),
                ));
            }
            let task_board_update = board_update(
                recorded.work_id.clone(),
                recorded.branch_id.clone(),
                None,
                changed_tasks,
            );
            if let Err(error) =
                executor.advance_active_primary_work_attempt(&active.attempt_id, successor)
            {
                return ToolResult::error(format!(
                    "Work task settled but local state could not advance: {error}"
                ));
            }
            ToolResult::text(
                json!({
                    "status": "recorded",
                    "work_id": recorded.work_id,
                    "branch_id": recorded.branch_id,
                    "item_id": recorded.item_id,
                    "item_revision": recorded.item_revision,
                    "attempt_id": recorded.attempt_id,
                    "outcome": recorded.outcome,
                    "blocker_kind": recorded.blocker_kind,
                    "unavailable_capabilities": recorded.unavailable_capabilities,
                    "execution_status": execution_status.as_str(),
                    "status_scope": "task_graph_execution",
                    "settlement_transition": settlement_transition,
                    "task_board_update": task_board_update,
                    "next_task": next_task,
                    "next_action": next_action,
                })
                .to_string(),
            )
        }
        Ok(PrimarySettlementResult::Recorded(recorded)) => ToolResult::text(
            json!({
                "status": "recorded",
                "work_id": recorded.work_id,
                "branch_id": recorded.branch_id,
                "item_id": recorded.item_id,
                "item_revision": recorded.item_revision,
                "attempt_id": recorded.attempt_id,
                "outcome": recorded.outcome,
                "blocker_kind": recorded.blocker_kind,
                "unavailable_capabilities": recorded.unavailable_capabilities,
            })
            .to_string(),
        ),
        Err(error) => ToolResult::error(format!("settle_work_item rejected: {error}")),
    }
}

enum PrimarySettlementResult {
    Advanced(astra_services::work::RecordedPrimaryWorkAttemptAdvance),
    Recorded(astra_services::work::RecordedWorkAttemptSettlement),
}

/// The bounded scheduler's state after one Work-item settlement. This is not
/// the Work delivery state: acceptance criteria and verification have their
/// own durable authority and must never be implied by task scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskGraphExecutionStatus {
    Active,
    NeedsRecovery,
    Blocked,
    Complete,
}

impl TaskGraphExecutionStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NeedsRecovery => "needs_recovery",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }
}

fn task_graph_execution_status(
    advance: &astra_services::work::PrimaryWorkAttemptAdvance,
) -> TaskGraphExecutionStatus {
    match advance {
        astra_services::work::PrimaryWorkAttemptAdvance::Assigned { .. } => {
            TaskGraphExecutionStatus::Active
        }
        astra_services::work::PrimaryWorkAttemptAdvance::NeedsRecovery => {
            TaskGraphExecutionStatus::NeedsRecovery
        }
        astra_services::work::PrimaryWorkAttemptAdvance::Blocked => {
            TaskGraphExecutionStatus::Blocked
        }
        astra_services::work::PrimaryWorkAttemptAdvance::Complete => {
            TaskGraphExecutionStatus::Complete
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InitialRunnableItem, InitialWorkItem, InitialWorkTask, StartWorkActivation, StartWorkArgs,
        TaskGraphExecutionStatus, WORK_ERROR_KIND_NOT_BOUND, board_settled_task,
        canonical_settlement_transition, canonical_start_work_payload,
        compile_continuation_task_graph, compile_initial_task_graph, confirmed_assignment,
        decode_canonical_work_establishment_payload, execute_run_next_work_item,
        execute_start_work, initial_declared_tasks, initial_runnable_items,
        start_work_operation_id, task_board_display_text, task_graph_execution_status,
        validate_initial_task_list,
    };
    use crate::server::runtime_tool_executor::ActivePrimaryWorkAttempt;
    use crate::server::runtime_tool_executor::RuntimeToolExecutor;
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::ensure_core_schema;
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, RecordedWorkAttemptSettlement,
        WorkAttemptOutcome, WorkEstablishmentPhase, WorkEstablishmentState, WorkItemAttemptId,
        WorkItemDeclarationState, WorkItemDelivery, WorkItemDeliveryStatus, WorkItemExecution,
        WorkItemExecutionStatus, WorkItemId, WorkItemKind, WorkItemRevision, WorkItemText,
        WorkOwnerId, WorkRepository, WorkRepositoryError, WorkTaskExecutionItem,
        WorkTaskExecutionNext,
    };
    use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use uuid::Uuid;

    static TEST_POOL: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();

    async fn setup_pool() -> SharedPool {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored Work lifecycle integration tests"
        );
        TEST_POOL
            .get_or_init(|| async {
                let settings = MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure canonical schema");
                SharedPool::new(&settings).await.expect("MatrixOne pool")
            })
            .await
            .clone()
    }

    async fn admit_running_test_session(
        pool: &SharedPool,
        owner: &WorkOwnerId,
        session: &InternalSessionId,
        run_id: &str,
        title: &str,
    ) {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, ?, 'active', 0)",
        )
        .bind(session.as_str())
        .bind(owner.as_str())
        .bind(title)
        .execute(pool.get())
        .await
        .expect("canonical session admission fence");
        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, root_run_id, ancestor_path, status,
              owner_pod_id, owner_lease_expires_at, run_generation)
             VALUES (?, ?, ?, ?, ?, 'running',
                     'work-lifecycle-test-owner', TIMESTAMPADD(MINUTE, 10, NOW(6)), 0)",
        )
        .bind(run_id)
        .bind(owner.as_str())
        .bind(session.as_str())
        .bind(run_id)
        .bind(run_id)
        .execute(pool.get())
        .await
        .expect("active run authority");
        sqlx::query(
            "INSERT INTO agent_session_execution_slots
             (user_id, session_id, run_id, acquired_at, updated_at)
             VALUES (?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(owner.as_str())
        .bind(session.as_str())
        .bind(run_id)
        .execute(pool.get())
        .await
        .expect("session execution slot");
    }

    fn replacement_start_work_args(goal: &str) -> Value {
        json!({
            "goal": goal,
            "activation": "start",
            "tasks": [{
                "objective": "Original outcome",
                "expected_result": "Original evidence"
            }]
        })
    }

    fn replacement_admission_decision(goal: &str) -> astra_services::WorkAdmissionDecision {
        astra_services::WorkAdmissionDecision::Required {
            domain: None,
            workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
            mutation_completion_scope: astra_config::user_profile::MutationCompletionScope::Unknown,
            goal: goal.to_string(),
            tasks: vec![astra_services::WorkAdmissionTask {
                objective: "Original outcome".to_string(),
                expected_result: "Original evidence".to_string(),
            }],
            deferred_graph_mutations: vec![astra_services::WorkAdmissionGraphMutation::Replace {
                target_initial_candidate: 1,
                target: astra_services::WorkAdmissionTask {
                    objective: "Original outcome".to_string(),
                    expected_result: "Original evidence".to_string(),
                },
                replacement: astra_services::WorkAdmissionTask {
                    objective: "Replacement outcome".to_string(),
                    expected_result: "Replacement evidence".to_string(),
                },
            }],
            activation: astra_services::WorkAdmissionActivation::Start,
            execution_topology: astra_services::WorkExecutionTopology::Primary,
            required_capabilities: Vec::new(),
        }
    }

    fn database_work_executor(
        workspace: &std::path::Path,
        pool: SharedPool,
        owner: &WorkOwnerId,
        session: &InternalSessionId,
    ) -> RuntimeToolExecutor {
        let mut executor = RuntimeToolExecutor::new(
            workspace.to_path_buf(),
            owner.as_str().to_string(),
            session.as_str().to_string(),
            None,
            None,
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            true, false,
        ));
        executor.set_context_manifest_pool(pool);
        executor
    }

    #[test]
    fn operation_identity_is_retry_stable_but_payload_scoped() {
        let args = StartWorkArgs {
            goal: "Track a bounded goal".to_string(),
            activation: StartWorkActivation::Start,
            tasks: vec![InitialWorkTask {
                objective: "Do one bounded thing".to_string(),
                expected_result: "One verifiable result".to_string(),
            }],
        };
        let owner = WorkOwnerId::parse("owner-1").expect("owner");
        let session = InternalSessionId::parse("session-1").expect("session");
        let other_session = InternalSessionId::parse("session-2").expect("session");
        let first = start_work_operation_id(&owner, &session, "turn-1", &args);
        assert_eq!(
            first,
            start_work_operation_id(&owner, &session, "turn-1", &args)
        );
        assert_ne!(
            first,
            start_work_operation_id(&owner, &other_session, "turn-1", &args)
        );
        assert_ne!(
            first,
            start_work_operation_id(&owner, &session, "turn-2", &args)
        );
        let mut changed = args;
        changed.tasks[0].expected_result = "A different result".to_string();
        assert_ne!(
            first,
            start_work_operation_id(&owner, &session, "turn-1", &changed)
        );
        assert!(first.starts_with("tool-"));
    }

    #[test]
    fn establishment_envelope_round_trips_deferred_graph_mutations() {
        let args = StartWorkArgs {
            goal: "Establish then redirect one outcome".to_string(),
            activation: StartWorkActivation::Start,
            tasks: vec![InitialWorkTask {
                objective: "Original outcome".to_string(),
                expected_result: "Original evidence".to_string(),
            }],
        };
        let decision = astra_services::WorkAdmissionDecision::Required {
            domain: None,
            workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
            mutation_completion_scope: astra_config::user_profile::MutationCompletionScope::Unknown,
            goal: args.goal.clone(),
            tasks: vec![astra_services::WorkAdmissionTask {
                objective: "Original outcome".to_string(),
                expected_result: "Original evidence".to_string(),
            }],
            deferred_graph_mutations: vec![astra_services::WorkAdmissionGraphMutation::Replace {
                target_initial_candidate: 1,
                target: astra_services::WorkAdmissionTask {
                    objective: "Original outcome".to_string(),
                    expected_result: "Original evidence".to_string(),
                },
                replacement: astra_services::WorkAdmissionTask {
                    objective: "Replacement outcome".to_string(),
                    expected_result: "Replacement evidence".to_string(),
                },
            }],
            activation: astra_services::WorkAdmissionActivation::Start,
            execution_topology: astra_services::WorkExecutionTopology::Primary,
            required_capabilities: Vec::new(),
        };
        let payload = canonical_start_work_payload("turn-1", &args, Some(&decision));
        let (decoded_args, decoded_decision) =
            decode_canonical_work_establishment_payload(&payload).expect("durable envelope");
        assert_eq!(decoded_args["goal"], args.goal);
        assert_eq!(decoded_decision, Some(decision));
    }

    #[test]
    fn task_graph_execution_status_is_complete_without_claiming_work_delivery() {
        let assigned = astra_services::work::PrimaryWorkAttemptAdvance::Assigned {
            attempt_id: WorkItemAttemptId::parse("attempt-1").expect("attempt"),
            item_id: WorkItemId::parse("task-1").expect("item"),
            item_revision: WorkItemRevision::INITIAL,
            objective: WorkItemText::parse("Run checks").expect("objective"),
            expected_result: WorkItemText::parse("Relevant checks have passed.")
                .expect("expected result"),
            resumed: false,
        };
        for (advance, expected, wire) in [
            (assigned, TaskGraphExecutionStatus::Active, "active"),
            (
                astra_services::work::PrimaryWorkAttemptAdvance::NeedsRecovery,
                TaskGraphExecutionStatus::NeedsRecovery,
                "needs_recovery",
            ),
            (
                astra_services::work::PrimaryWorkAttemptAdvance::Blocked,
                TaskGraphExecutionStatus::Blocked,
                "blocked",
            ),
            (
                astra_services::work::PrimaryWorkAttemptAdvance::Complete,
                TaskGraphExecutionStatus::Complete,
                "complete",
            ),
        ] {
            let actual = task_graph_execution_status(&advance);
            assert_eq!(actual, expected);
            assert_eq!(actual.as_str(), wire);
        }
    }

    #[test]
    fn establishment_accepts_only_an_exact_durable_assignment_receipt() {
        let temp = TempDir::new().expect("temp workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        executor
            .install_active_primary_work_attempt(ActivePrimaryWorkAttempt {
                attempt_id: "attempt-1".to_string(),
                executor_run_id: "run-1".to_string(),
                item_id: "task-1".to_string(),
                item_revision: 1,
                objective: "Do the task".to_string(),
                expected_result: "The task is verified".to_string(),
            })
            .expect("active attempt");

        for payload in [
            json!({"status": "complete"}),
            json!({"status": "assigned", "attempt_id": "wrong", "item_id": "task-1", "item_revision": 1}),
            json!({"status": "assigned", "attempt_id": "attempt-1", "item_id": "task-2", "item_revision": 1}),
        ] {
            assert!(
                confirmed_assignment(
                    &executor,
                    "run-1",
                    astra_tools::ToolResult::text(payload.to_string()),
                )
                .is_err()
            );
        }
        let exact = confirmed_assignment(
            &executor,
            "run-1",
            astra_tools::ToolResult::text(
                json!({
                    "status": "assigned",
                    "attempt_id": "attempt-1",
                    "item_id": "task-1",
                    "item_revision": 1,
                })
                .to_string(),
            ),
        )
        .expect("exact durable assignment");
        assert_eq!(exact["status"], "assigned");
    }

    #[test]
    fn settlement_receipt_exposes_canonical_item_transition_not_summary_claims() {
        let active = ActivePrimaryWorkAttempt {
            attempt_id: "attempt-1".to_string(),
            executor_run_id: "run-1".to_string(),
            item_id: "task-1".to_string(),
            item_revision: 2,
            objective: "Fetch one headline".to_string(),
            expected_result: "One sourced headline".to_string(),
        };
        let recorded = RecordedWorkAttemptSettlement {
            run_id: "run-1".to_string(),
            work_id: "work-1".to_string(),
            branch_id: "main".to_string(),
            item_id: "task-1".to_string(),
            item_revision: 2,
            attempt_id: "attempt-1".to_string(),
            outcome: WorkAttemptOutcome::Delivered,
            summary: "Arbitrary contradictory progress prose".to_string(),
            blocker_kind: None,
            unavailable_capabilities: Vec::new(),
        };

        let transition = canonical_settlement_transition(&board_settled_task(&active, &recorded));

        assert_eq!(transition["authority"], "canonical_work_state");
        assert_eq!(transition["declaration_state"], "active");
        assert_eq!(transition["execution_status"], "completed");
        assert_eq!(transition["delivery_status"], "delivered");
        assert_eq!(
            transition["summary_authority"],
            "non_authoritative_progress_note"
        );
        assert!(transition.get("summary").is_none());
    }

    #[test]
    fn initial_task_list_compiles_without_inventing_dependencies() {
        let tasks = vec![
            InitialWorkTask {
                objective: "Inspect the narrow command surface".to_string(),
                expected_result: "One cited command finding".to_string(),
            },
            InitialWorkTask {
                objective: "Trace the corresponding client route".to_string(),
                expected_result: "One cited route finding".to_string(),
            },
        ];
        let (items, dependencies) = compile_initial_task_graph(&tasks);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_id, "task-1");
        assert_eq!(items[1].item_id, "task-2");
        assert_eq!(items[0].kind, "task");
        assert_eq!(items[1].kind, "task");
        assert!(
            dependencies.is_empty(),
            "semantic admission did not establish a precedence relationship"
        );
        assert_eq!(
            initial_runnable_items(&items),
            vec![InitialRunnableItem {
                item_id: "task-1".to_string(),
                item_revision: 1,
            }]
        );
        assert_eq!(
            initial_declared_tasks(&items)
                .iter()
                .map(|task| (task.item_id.as_str(), task.item_revision))
                .collect::<Vec<_>>(),
            vec![("task-1", 1), ("task-2", 1)]
        );
    }

    #[test]
    fn continuation_task_ids_are_server_owned_and_retry_stable() {
        let tasks = vec![
            InitialWorkTask {
                objective: "Fetch the next bounded result".to_string(),
                expected_result: "One cited result".to_string(),
            },
            InitialWorkTask {
                objective: "Verify the result".to_string(),
                expected_result: "One deterministic verification".to_string(),
            },
        ];
        let items = compile_continuation_task_graph("operation-1", &tasks);
        let retried_after_graph_changed = compile_continuation_task_graph("operation-1", &tasks);
        let different_operation = compile_continuation_task_graph("operation-2", &tasks);

        assert_eq!(items, retried_after_graph_changed);
        assert_ne!(items[0].item_id, different_operation[0].item_id);
        assert_ne!(items[0].item_id, items[1].item_id);
        assert!(items.iter().all(|item| item.item_id.len() <= 64));
        assert_eq!(items[0].objective, tasks[0].objective);
        assert_eq!(items[1].expected_result, tasks[1].expected_result);
    }

    #[test]
    fn live_board_receipt_is_compact_utf8_safe_display_projection() {
        let emoji = "🧪".repeat(200);
        let displayed = task_board_display_text(&emoji);
        assert!(displayed.len() <= astra_server_types::WORK_TASK_BOARD_TEXT_MAX_BYTES);
        assert!(displayed.ends_with('…'));
        assert!(displayed.is_char_boundary(displayed.len()));
    }

    #[test]
    fn initial_task_list_contract_rejects_model_authored_graph_mechanics() {
        for input in [
            json!({
                "goal": "Ship a verified change",
                "activation": "start",
                "items": [],
                "dependencies": []
            }),
            json!({
                "goal": "Ship a verified change",
                "activation": "start",
                "tasks": [{
                    "item_id": "model-picked-id",
                    "objective": "Inspect the current behavior",
                    "expected_result": "One reproducible observation"
                }]
            }),
        ] {
            assert!(
                serde_json::from_value::<StartWorkArgs>(input).is_err(),
                "the start contract must accept semantic tasks, not model-authored graph mechanics"
            );
        }
    }

    #[test]
    fn initial_task_list_rejects_invalid_task_text_before_storage() {
        assert!(
            validate_initial_task_list(&[InitialWorkTask {
                objective: " ".to_string(),
                expected_result: "One result".to_string(),
            }])
            .is_err()
        );
    }

    #[tokio::test]
    async fn initial_task_list_bounds_fail_before_identity_or_storage_side_effects() {
        let temp = TempDir::new().expect("workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        for tasks in [
            Vec::new(),
            (0..9)
                .map(|_| {
                    json!({
                        "objective": "Do one bounded thing",
                        "expected_result": "One verifiable result"
                    })
                })
                .collect(),
        ] {
            let result = execute_start_work(
                &executor,
                &json!({
                    "goal": "Track a bounded goal",
                    "activation": "start",
                    "tasks": tasks
                }),
                ToolInvocationMetadata::default(),
            )
            .await;
            assert!(result.is_error);
            assert!(result.output.contains("bounded lifecycle contract"));
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn terminal_establishment_replay_preserves_mutations_and_assignment_once() {
        let pool = setup_pool().await;
        let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
        let session =
            InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
        let run_id = format!("run-{}", Uuid::new_v4());
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
        admit_running_test_session(&pool, &owner, &session, &run_id, "Work lifecycle replay").await;
        let args = replacement_start_work_args("Replace the initial outcome before execution");
        let decision =
            replacement_admission_decision("Replace the initial outcome before execution");
        let request = super::canonical_work_establishment_request(
            &owner,
            &session,
            "turn-1",
            &run_id,
            &args,
            Some(&decision),
        )
        .expect("canonical request");
        astra_services::work::DatabaseWorkEstablishmentService::new(pool.clone())
            .admit(&request)
            .await
            .expect("durable admission");

        let temp = TempDir::new().expect("workspace");
        let mut executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            owner.as_str().to_string(),
            session.as_str().to_string(),
            None,
            None,
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            true, false,
        ));
        executor.set_context_manifest_pool(pool.clone());
        executor
            .bind_work_establishment_operation("physical-1", &request.operation_id)
            .expect("trusted recovery provenance");
        let mismatched = execute_start_work(
            &executor,
            &json!({
                "goal": "A different operation must not borrow trusted provenance",
                "activation": "defer",
                "tasks": [{
                    "objective": "Different outcome",
                    "expected_result": "Different evidence"
                }]
            }),
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("turn-2"),
                tool_call_id: Some("physical-1"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(mismatched.is_error);
        assert!(mismatched.output.contains("immutable durable payload"));
        let first = execute_start_work(
            &executor,
            &args,
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("turn-2"),
                tool_call_id: Some("physical-1"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(!first.is_error, "first establishment: {first:?}");
        let first: Value = serde_json::from_str(&first.output).expect("first receipt");
        assert_eq!(first["status"], "started");
        assert_eq!(first["initial_task"]["objective"], "Replacement outcome");

        executor
            .bind_work_establishment_operation("physical-2", &request.operation_id)
            .expect("trusted replay provenance");
        let replay = execute_start_work(
            &executor,
            &args,
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("turn-2"),
                tool_call_id: Some("physical-2"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(!replay.is_error, "terminal replay: {replay:?}");
        let replay: Value = serde_json::from_str(&replay.output).expect("replay receipt");
        assert_eq!(replay["status"], "started");
        assert_eq!(replay["replayed"], true);

        let binding = executor.work_binding.get().expect("binding");
        let snapshot = binding
            .repository
            .load_task_execution_snapshot_for_session(&owner, &session)
            .await
            .expect("canonical snapshot");
        assert_eq!(snapshot.items().len(), 3, "root + original + replacement");
        assert_eq!(
            snapshot
                .items()
                .iter()
                .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
                .count(),
            1,
            "replay must not create a second assignment"
        );

        let deferred_session = InternalSessionId::parse(format!("session-{}", Uuid::new_v4()))
            .expect("deferred session");
        let deferred_run_id = format!("run-{}", Uuid::new_v4());
        admit_running_test_session(
            &pool,
            &owner,
            &deferred_session,
            &deferred_run_id,
            "Work lifecycle defer",
        )
        .await;
        let deferred_args = json!({
            "goal": "Do not activate the next declaration yet",
            "activation": "defer",
            "tasks": [{
                "objective": "Future outcome",
                "expected_result": "Future evidence"
            }]
        });
        let deferred_request = super::canonical_work_establishment_request(
            &owner,
            &deferred_session,
            "turn-3",
            &deferred_run_id,
            &deferred_args,
            None,
        )
        .expect("deferred canonical request");
        let establishment = DatabaseWorkEstablishmentService::new(pool.clone());
        establishment
            .admit(&deferred_request)
            .await
            .expect("pending operation before typed defer");
        let deferred_temp = TempDir::new().expect("deferred workspace");
        let mut deferred_executor = RuntimeToolExecutor::new(
            deferred_temp.path().to_path_buf(),
            owner.as_str().to_string(),
            deferred_session.as_str().to_string(),
            None,
            None,
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            true, false,
        ));
        deferred_executor.set_context_manifest_pool(pool.clone());
        deferred_executor
            .bind_work_establishment_defer("physical-defer", &deferred_request.operation_id)
            .expect("typed defer provenance");
        let deferred = execute_start_work(
            &deferred_executor,
            &deferred_args,
            ToolInvocationMetadata {
                run_id: Some(&deferred_run_id),
                turn_chain_id: Some("turn-4"),
                tool_call_id: Some("physical-defer"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(!deferred.is_error, "typed defer: {deferred:?}");
        let deferred: Value = serde_json::from_str(&deferred.output).expect("defer receipt");
        assert_eq!(deferred["status"], "deferred");
        assert_eq!(deferred["assignment_created"], false);
        assert_eq!(
            establishment
                .load(&owner, &deferred_request.operation_id)
                .await
                .expect("cancelled operation")
                .state,
            WorkEstablishmentState::Cancelled
        );
        assert!(matches!(
            astra_services::work::DatabaseWorkRepository::new(pool.clone())
                .load_session_plan_binding(&owner, &deferred_session)
                .await,
            Err(WorkRepositoryError::NotFound)
        ));

        let plan_only_request = super::canonical_work_establishment_request(
            &owner,
            &deferred_session,
            "turn-plan-only",
            &deferred_run_id,
            &deferred_args,
            None,
        )
        .expect("plan-only request");
        establishment
            .admit(&plan_only_request)
            .await
            .expect("pending plan-only operation");
        deferred_executor
            .bind_work_establishment_operation(
                "physical-plan-only",
                &plan_only_request.operation_id,
            )
            .expect("plan-only establishment provenance");
        let plan_only = execute_start_work(
            &deferred_executor,
            &deferred_args,
            ToolInvocationMetadata {
                run_id: Some(&deferred_run_id),
                turn_chain_id: Some("turn-after-restart"),
                tool_call_id: Some("physical-plan-only"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(
            !plan_only.is_error,
            "recovered plan-only establishment: {plan_only:?}"
        );
        let plan_only: Value = serde_json::from_str(&plan_only.output).expect("plan-only receipt");
        assert_eq!(plan_only["status"], "started");
        assert_eq!(plan_only["activation"], "defer");
        assert!(plan_only["initial_task"].is_null());
        assert_eq!(
            establishment
                .load(&owner, &plan_only_request.operation_id)
                .await
                .expect("completed plan-only operation")
                .state,
            WorkEstablishmentState::Complete
        );
        let plan_only_snapshot = deferred_executor
            .work_binding
            .get()
            .expect("plan-only binding")
            .repository
            .load_task_execution_snapshot_for_session(&owner, &deferred_session)
            .await
            .expect("plan-only snapshot");
        assert!(plan_only_snapshot.items().iter().all(|item| !matches!(
            item.execution.status,
            WorkItemExecutionStatus::Running | WorkItemExecutionStatus::Paused
        )));

        sqlx::query(
            "UPDATE work_establishment_operations
             SET operation_state = 'pending', operation_phase = 'awaiting_assignment'
             WHERE owner_id = ? AND operation_id = ?",
        )
        .bind(owner.as_str())
        .bind(&request.operation_id)
        .execute(pool.get())
        .await
        .expect("fault-inject assignment-before-complete crash window");
        executor
            .bind_work_establishment_defer("physical-active-defer", &request.operation_id)
            .expect("active-window defer provenance");
        let active_defer = execute_start_work(
            &executor,
            &args,
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("turn-5"),
                tool_call_id: Some("physical-active-defer"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(active_defer.is_error);
        assert!(active_defer.output.contains("durable active attempt"));
        let active_window = establishment
            .load(&owner, &request.operation_id)
            .await
            .expect("active crash-window operation");
        assert_eq!(active_window.state, WorkEstablishmentState::Pending);
        assert_eq!(
            active_window.phase,
            WorkEstablishmentPhase::AwaitingAssignment
        );

        executor
            .bind_work_establishment_operation("physical-assignment-replay", &request.operation_id)
            .expect("assignment-phase recovery provenance");
        let assignment_replay = execute_start_work(
            &executor,
            &args,
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("turn-6"),
                tool_call_id: Some("physical-assignment-replay"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(
            !assignment_replay.is_error,
            "assignment-phase replay: {assignment_replay:?}"
        );
        let assignment_replay: Value =
            serde_json::from_str(&assignment_replay.output).expect("assignment replay receipt");
        assert_eq!(assignment_replay["status"], "started");
        assert_eq!(
            establishment
                .load(&owner, &request.operation_id)
                .await
                .expect("completed assignment-phase replay")
                .state,
            WorkEstablishmentState::Complete
        );
        let assignment_snapshot = executor
            .work_binding
            .get()
            .expect("assignment replay binding")
            .repository
            .load_task_execution_snapshot_for_session(&owner, &session)
            .await
            .expect("assignment replay snapshot");
        assert_eq!(assignment_snapshot.items().len(), 3);
        assert_eq!(
            assignment_snapshot
                .items()
                .iter()
                .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
                .count(),
            1,
            "recovery after mutation commit must preserve one assignment"
        );
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn pending_establishment_replay_accepts_its_exact_genesis_binding() {
        let pool = setup_pool().await;
        let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;

        for (case, durable_phase) in [
            ("awaiting-genesis", WorkEstablishmentPhase::AwaitingGenesis),
            ("awaiting-plan", WorkEstablishmentPhase::AwaitingPlan),
        ] {
            let session =
                InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
            let run_id = format!("run-{}", Uuid::new_v4());
            admit_running_test_session(&pool, &owner, &session, &run_id, case).await;

            let goal = format!("Recover exact operation from {case}");
            let args = replacement_start_work_args(&goal);
            let decision = replacement_admission_decision(&goal);
            let request = super::canonical_work_establishment_request(
                &owner,
                &session,
                "original-turn",
                &run_id,
                &args,
                Some(&decision),
            )
            .expect("canonical recovery request");
            let establishment = DatabaseWorkEstablishmentService::new(pool.clone());
            establishment
                .admit(&request)
                .await
                .expect("durable operation admission");

            // Fault-inject the crash window after genesis committed but
            // before either the genesis or plan phase was durably advanced.
            // The resulting session binding belongs to this exact operation.
            let creation = crate::server::work_handlers::derive_work_creation(
                &owner,
                astra_server_types::WorkCreateRequestV1 {
                    request_id: request.operation_id.clone(),
                    goal: goal.clone(),
                    criteria: Vec::new(),
                },
            )
            .expect("operation-owned genesis");
            assert_eq!(creation.work_id, request.work_id);
            assert_eq!(creation.branch_id, request.branch_id);
            let repository = astra_services::work::DatabaseWorkRepository::new(pool.clone());
            repository
                .create_genesis_in_running_session(
                    creation.genesis.in_session(session.clone()),
                    &run_id,
                )
                .await
                .expect("committed genesis before crash");
            if durable_phase == WorkEstablishmentPhase::AwaitingPlan {
                establishment
                    .advance_phase(
                        &request,
                        WorkEstablishmentPhase::AwaitingGenesis,
                        WorkEstablishmentPhase::AwaitingPlan,
                    )
                    .await
                    .expect("fault-injected durable plan boundary");
            }

            let temp = TempDir::new().expect("workspace");
            let executor = database_work_executor(temp.path(), pool.clone(), &owner, &session);
            let physical_call_id = format!("physical-recovery-{case}");
            executor
                .bind_work_establishment_operation(&physical_call_id, &request.operation_id)
                .expect("trusted recovery provenance");
            let recovered = execute_start_work(
                &executor,
                &args,
                ToolInvocationMetadata {
                    run_id: Some(&run_id),
                    turn_chain_id: Some("recovery-turn"),
                    tool_call_id: Some(&physical_call_id),
                    admission_source: Some(ToolInvocationAdmissionSource::Policy),
                    expected_control_epoch: None,
                },
            )
            .await;
            assert!(!recovered.is_error, "{case} recovery: {recovered:?}");
            let recovered: Value =
                serde_json::from_str(&recovered.output).expect("structured recovery receipt");
            assert_eq!(recovered["status"], "started");

            let operation = establishment
                .load(&owner, &request.operation_id)
                .await
                .expect("completed recovery operation");
            assert_eq!(operation.state, WorkEstablishmentState::Complete);
            assert_eq!(operation.phase, WorkEstablishmentPhase::Complete);
            let snapshot = repository
                .load_task_execution_snapshot_for_session(&owner, &session)
                .await
                .expect("recovered graph");
            assert_eq!(snapshot.items().len(), 3, "root + original + replacement");
            let original = snapshot
                .items()
                .iter()
                .find(|item| item.item_id.as_str() == "task-1")
                .expect("original candidate");
            assert_eq!(
                original.declaration_state,
                WorkItemDeclarationState::Superseded
            );
            assert_eq!(
                snapshot
                    .items()
                    .iter()
                    .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
                    .count(),
                1,
                "each recovery phase must create exactly one assignment"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn pending_establishment_rejects_a_foreign_session_binding() {
        let pool = setup_pool().await;
        let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
        let session =
            InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
        let run_id = format!("run-{}", Uuid::new_v4());
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
        admit_running_test_session(
            &pool,
            &owner,
            &session,
            &run_id,
            "Foreign binding recovery fence",
        )
        .await;

        let goal = "Do not apply operation mutations to a foreign graph";
        let args = replacement_start_work_args(goal);
        let decision = replacement_admission_decision(goal);
        let request = super::canonical_work_establishment_request(
            &owner,
            &session,
            "original-turn",
            &run_id,
            &args,
            Some(&decision),
        )
        .expect("canonical pending operation");
        let establishment = DatabaseWorkEstablishmentService::new(pool.clone());
        establishment
            .admit(&request)
            .await
            .expect("pending operation admission");

        let foreign_creation = crate::server::work_handlers::derive_work_creation(
            &owner,
            astra_server_types::WorkCreateRequestV1 {
                request_id: format!("foreign-{}", Uuid::new_v4()),
                goal: "Foreign already-bound Work".to_string(),
                criteria: Vec::new(),
            },
        )
        .expect("foreign genesis");
        assert_ne!(foreign_creation.work_id, request.work_id);
        let foreign_work_id = foreign_creation.work_id.clone();
        let repository = astra_services::work::DatabaseWorkRepository::new(pool.clone());
        repository
            .create_genesis_in_running_session(
                foreign_creation.genesis.in_session(session.clone()),
                &run_id,
            )
            .await
            .expect("foreign session binding");

        let temp = TempDir::new().expect("workspace");
        let executor = database_work_executor(temp.path(), pool.clone(), &owner, &session);
        executor
            .bind_work_establishment_operation("physical-foreign-fence", &request.operation_id)
            .expect("trusted pending provenance");
        let rejected = execute_start_work(
            &executor,
            &args,
            ToolInvocationMetadata {
                run_id: Some(&run_id),
                turn_chain_id: Some("recovery-turn"),
                tool_call_id: Some("physical-foreign-fence"),
                admission_source: Some(ToolInvocationAdmissionSource::Policy),
                expected_control_epoch: None,
            },
        )
        .await;
        assert!(rejected.is_error);
        let rejection: Value =
            serde_json::from_str(&rejected.output).expect("typed binding rejection");
        assert_eq!(rejection["status"], "rejected");
        assert_eq!(
            rejection["error_kind"],
            "work_establishment_binding_changed"
        );
        assert_eq!(rejection["next_action"], "refresh_work_state");

        let operation = establishment
            .load(&owner, &request.operation_id)
            .await
            .expect("pending operation remains recoverable");
        assert_eq!(operation.state, WorkEstablishmentState::Pending);
        assert_eq!(operation.phase, WorkEstablishmentPhase::AwaitingGenesis);
        let binding = repository
            .load_session_plan_binding(&owner, &session)
            .await
            .expect("foreign binding remains canonical");
        assert_eq!(binding.work_id, foreign_work_id);
        let snapshot = repository
            .load_task_execution_snapshot_for_session(&owner, &session)
            .await
            .expect("foreign graph remains unchanged");
        assert_eq!(snapshot.items().len(), 1, "only the foreign root exists");
    }

    #[tokio::test]
    async fn run_next_without_work_binding_returns_typed_start_work_transition() {
        let temp = TempDir::new().expect("workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        let result = execute_run_next_work_item(
            &executor,
            &json!({}),
            ToolInvocationMetadata {
                run_id: Some("run-1"),
                tool_call_id: Some("call-1"),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_error);
        let output: Value = serde_json::from_str(&result.output).expect("typed Work error");
        assert_eq!(output["status"], "rejected");
        assert_eq!(output["error_kind"], WORK_ERROR_KIND_NOT_BOUND);
        assert_eq!(output["next_action"], "start_work");
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(Value::as_str),
            Some(WORK_ERROR_KIND_NOT_BOUND)
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("next_action"))
                .and_then(Value::as_str),
            Some("start_work")
        );
    }
}
