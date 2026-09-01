use astra_server_types::{
    WORK_TASK_BOARD_TEXT_MAX_BYTES, WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION, WorkCreateRequestV1,
    WorkTaskBoardBlockerKindV1, WorkTaskBoardChangeV1, WorkTaskBoardDeclarationStateV1,
    WorkTaskBoardDeliveryStatusV1, WorkTaskBoardExecutionStatusV1, WorkTaskBoardTaskV1,
    WorkTaskBoardUpdateV1,
};
use astra_services::work::{
    InternalSessionId, NewWorkAttemptSettlement, NewWorkItemAttempt, WorkAttemptExecutionMode,
    WorkAttemptOutcome, WorkItemAttemptId, WorkItemExecutionStatus, WorkItemId, WorkItemRevision,
    WorkItemRevisionRef, WorkItemText, WorkOwnerId, WorkRepository, WorkRepositoryError,
    WorkTaskExecutionNext,
};
use astra_tools::ToolResult;
use astra_tools::tool_engine::ToolInvocationMetadata;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use super::runtime_tool_executor::{
    ActivePrimaryWorkAttempt, RuntimeToolExecutor, WorkRuntimeBinding,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartWorkArgs {
    goal: String,
    activation: StartWorkActivation,
    tasks: Vec<InitialWorkTask>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StartWorkActivation {
    Start,
    Defer,
}

#[derive(Clone, Serialize)]
struct InitialWorkItem {
    item_id: String,
    kind: &'static str,
    objective: String,
    expected_result: String,
}

#[derive(Clone, Deserialize)]
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

/// Build the compact live-board snapshot from the same server-owned task
/// allocation that was just committed.  The model never provides ids, task
/// states, or delivery facts, and this remains bounded by `start_work`'s
/// eight-task contract.
fn initial_task_board_snapshot(
    work_id: &str,
    branch_id: &str,
    goal: String,
    graph_revision: i64,
    items: &[InitialWorkItem],
    active_item_id: Option<&str>,
) -> WorkTaskBoardUpdateV1 {
    WorkTaskBoardUpdateV1 {
        schema_version: WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION,
        work_id: work_id.to_string(),
        branch_id: branch_id.to_string(),
        change: WorkTaskBoardChangeV1::Snapshot {
            goal: task_board_display_text(&goal),
            graph_revision,
            criteria_member_count: 0,
            tasks: items
                .iter()
                .map(|item| WorkTaskBoardTaskV1 {
                    item_id: item.item_id.clone(),
                    item_revision: WorkItemRevision::INITIAL.get(),
                    objective: task_board_display_text(&item.objective),
                    expected_result: task_board_display_text(&item.expected_result),
                    declaration_state: WorkTaskBoardDeclarationStateV1::Active,
                    execution_status: if Some(item.item_id.as_str()) == active_item_id {
                        WorkTaskBoardExecutionStatusV1::Running
                    } else {
                        WorkTaskBoardExecutionStatusV1::NotStarted
                    },
                    delivery_status: WorkTaskBoardDeliveryStatusV1::Unreported,
                    delivery_summary: None,
                    blocker_kind: None,
                    unavailable_capabilities: Vec::new(),
                })
                .collect(),
        },
    }
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

/// Allocate follow-up task identities in the server-owned task namespace.
///
/// A second `start_work` call is a continuation of the session's existing
/// Work branch, not a request to recreate genesis. Existing plans may contain
/// arbitrary server-issued ids, so allocation checks the complete immutable
/// context and picks the first unused `task-N` identity. The model supplies
/// only semantic text; it never controls graph identity.
fn compile_continuation_task_graph(
    existing_item_ids: &[String],
    tasks: &[InitialWorkTask],
) -> Vec<InitialWorkItem> {
    let mut used: HashSet<String> = existing_item_ids.iter().cloned().collect();
    let mut next_number = 1usize;
    tasks
        .iter()
        .map(|task| {
            let item_id = loop {
                let candidate = format!("task-{next_number}");
                next_number = next_number.saturating_add(1);
                if used.insert(candidate.clone()) {
                    break candidate;
                }
            };
            InitialWorkItem {
                item_id,
                kind: "task",
                objective: task.objective.clone(),
                expected_result: task.expected_result.clone(),
            }
        })
        .collect()
}

fn continuation_task_board_update(
    work_id: &str,
    branch_id: &str,
    graph_revision: i64,
    items: &[InitialWorkItem],
    active_item_id: Option<&str>,
) -> WorkTaskBoardUpdateV1 {
    board_update(
        work_id.to_string(),
        branch_id.to_string(),
        Some(graph_revision),
        items
            .iter()
            .map(|item| WorkTaskBoardTaskV1 {
                item_id: item.item_id.clone(),
                item_revision: WorkItemRevision::INITIAL.get(),
                objective: task_board_display_text(&item.objective),
                expected_result: task_board_display_text(&item.expected_result),
                declaration_state: WorkTaskBoardDeclarationStateV1::Active,
                execution_status: if Some(item.item_id.as_str()) == active_item_id {
                    WorkTaskBoardExecutionStatusV1::Running
                } else {
                    WorkTaskBoardExecutionStatusV1::NotStarted
                },
                delivery_status: WorkTaskBoardDeliveryStatusV1::Unreported,
                delivery_summary: None,
                blocker_kind: None,
                unavailable_capabilities: Vec::new(),
            })
            .collect(),
    )
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

fn start_work_request_id(run_id: &str, tool_call_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"start-work-tool-v1\0");
    digest.update(run_id.as_bytes());
    digest.update(b"\0");
    digest.update(tool_call_id.as_bytes());
    format!("tool-{:x}", digest.finalize())
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
async fn already_bound_work_receipt(
    binding: &WorkRuntimeBinding,
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
    let (status, error_kind, next_action) = if same_identity {
        (
            "already_started",
            "canonical_work_already_started",
            "inspect_existing_work_or_run_next_work_item",
        )
    } else {
        (
            "already_bound",
            "canonical_work_already_bound",
            "inspect_work_plan_then_propose_work_plan",
        )
    };
    ToolResult::text(
        json!({
            "status": status,
            "error_kind": error_kind,
            "retryable": false,
            "work_id": binding.work_id,
            "branch_id": binding.branch_id,
            "graph_revision": snapshot.basis().graph_revision,
            "graph_state": graph_state,
            "item_count": snapshot.items().len(),
            "requested_goal": requested_goal,
            "next_action": next_action,
            "instruction": if same_identity {
                "This start_work request was already applied. Do not replay genesis; inspect the current Work state and continue from its typed next action."
            } else {
                "This session already owns one canonical Work branch. Do not create a second Work. Inspect the current graph, then use propose_work_plan to add or revise items for the new user goal."
            },
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
    requested_goal: &str,
    tasks: &[InitialWorkTask],
    activation: StartWorkActivation,
    invocation: ToolInvocationMetadata<'_>,
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
    if context.items().len().saturating_add(tasks.len()) > 256 {
        return ToolResult::error(
            "canonical Work continuation exceeds the bounded 256-item graph contract".to_string(),
        );
    }
    let existing_item_ids = context
        .items()
        .iter()
        .map(|item| item.item_id.as_str().to_string())
        .collect::<Vec<_>>();
    let additions = compile_continuation_task_graph(&existing_item_ids, tasks);
    let board_items = additions.clone();
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
        invocation,
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
    let Some(graph_revision) = proposal_output
        .get("result_graph_revision")
        .and_then(Value::as_i64)
        .filter(|revision| *revision > 0)
    else {
        return ToolResult::error(
            "canonical Work continuation returned no valid graph revision".to_string(),
        );
    };
    let (next_task, dispatch_error, next_action) = match activation {
        StartWorkActivation::Defer => (None, None, "await_explicit_continuation"),
        StartWorkActivation::Start => {
            let assignment = execute_run_next_work_item(executor, &json!({}), invocation).await;
            if assignment.is_error {
                (
                    None,
                    Some(assignment.output),
                    "run_next_work_item_to_recover_dispatch",
                )
            } else {
                match serde_json::from_str::<Value>(&assignment.output) {
                    Ok(assignment) => (
                        Some(assignment),
                        None,
                        "execute_assigned_task_then_call_settle_work_item",
                    ),
                    Err(_) => (
                        None,
                        Some("continuation assignment returned invalid structured state".into()),
                        "run_next_work_item_to_recover_dispatch",
                    ),
                }
            }
        }
    };
    let active_item_id = next_task
        .as_ref()
        .and_then(|task| task.get("item_id"))
        .and_then(Value::as_str);
    let board_update = continuation_task_board_update(
        binding.work_id.as_str(),
        binding.branch_id.as_str(),
        graph_revision,
        &board_items,
        active_item_id,
    );
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
    let (initial_items, initial_dependencies) = compile_initial_task_graph(&args.tasks);
    let activation = args.activation;
    let Some(run_id) = invocation.run_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return ToolResult::error(
            "start_work requires the exact current durable run identity".to_string(),
        );
    };
    let Some(tool_call_id) = invocation
        .tool_call_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return ToolResult::error(
            "start_work requires the exact current tool-call identity".to_string(),
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
        request_id: start_work_request_id(run_id, tool_call_id),
        goal: args.goal,
        // Model-authored Done-when remains provisional. Starting Work never
        // silently accepts criteria on the user's behalf.
        criteria: Vec::new(),
    };
    let creation = match super::work_handlers::derive_work_creation(&owner_id, request) {
        Ok(creation) => creation,
        Err(_) => return ToolResult::error("start_work goal is invalid or too large".to_string()),
    };
    let repository = astra_services::work::DatabaseWorkRepository::new(pool.clone());
    if let Some(binding) = executor.work_binding.get() {
        let same_identity =
            binding.work_id == creation.work_id && binding.branch_id == creation.branch_id;
        if same_identity {
            return already_bound_work_receipt(binding, &work_goal, true).await;
        }
        return continue_bound_work(
            executor,
            binding,
            &work_goal,
            &args.tasks,
            activation,
            invocation,
        )
        .await;
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
                    Ok(existing)
                        if existing.work_id == creation.work_id
                            && existing.branch_id == creation.branch_id => {}
                    Ok(existing) => {
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
                        let binding = executor
                            .work_binding
                            .get()
                            .expect("Work binding was installed above");
                        return continue_bound_work(
                            executor,
                            binding,
                            &work_goal,
                            &args.tasks,
                            activation,
                            invocation,
                        )
                        .await;
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
            pool,
            owner_id,
            session_id,
            creation.work_id.clone(),
            creation.branch_id.clone(),
        )) {
            return ToolResult::error(format!(
                "Work was created but the current run could not bind its planning surface: {error}"
            ));
        }
    }

    // Establishment and initial decomposition are one model-facing action.
    // Reuse the exact Work proposal seam so graph validation, owner isolation,
    // optimistic concurrency, admission, and retries have one implementation.
    let inspected = super::tool_work_plan::inspect(executor, &json!({}), None).await;
    if inspected.is_error {
        return inspected;
    }
    let inspected: Value = match serde_json::from_str(&inspected.output) {
        Ok(value) => value,
        Err(_) => {
            return ToolResult::error(
                "Work was created but its initial planning context was invalid".to_string(),
            );
        }
    };
    let Some(context_id) = inspected.get("context_id").and_then(Value::as_str) else {
        return ToolResult::error(
            "Work was created but its initial planning context had no identity".to_string(),
        );
    };
    let initial_item_count = initial_items.len();
    let runnable_items = initial_runnable_items(&initial_items);
    let declared_tasks = initial_declared_tasks(&initial_items);
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
        invocation,
        None,
    )
    .await;
    if proposal.is_error {
        return proposal;
    }
    let proposal_output: Value = serde_json::from_str(&proposal.output).unwrap_or(Value::Null);
    let Some(graph_revision) = proposal_output
        .get("result_graph_revision")
        .and_then(Value::as_u64)
        .filter(|revision| *revision > 0 && *revision <= i64::MAX as u64)
        .map(|revision| revision as i64)
    else {
        return ToolResult::error(
            "Work initial planning returned no valid graph revision".to_string(),
        );
    };
    // Ordinary Work establishment and its first executable assignment are
    // one product action. Explicitly deferred Work remains durably Ready and
    // owns no attempt until a later typed run_next_work_item call.
    let (initial_task, dispatch_error, next_action) = match activation {
        StartWorkActivation::Defer => (None, None, "await_explicit_continuation"),
        StartWorkActivation::Start => {
            let initial_assignment =
                execute_run_next_work_item(executor, &json!({}), invocation).await;
            if initial_assignment.is_error {
                (
                    None,
                    Some(initial_assignment.output),
                    "run_next_work_item_to_recover_initial_dispatch",
                )
            } else {
                match serde_json::from_str::<Value>(&initial_assignment.output) {
                    Ok(assignment) => (
                        Some(assignment),
                        None,
                        "execute_initial_task_then_call_settle_work_item",
                    ),
                    Err(_) => (
                        None,
                        Some(
                            "initial Work assignment returned invalid structured state".to_string(),
                        ),
                        "run_next_work_item_to_recover_initial_dispatch",
                    ),
                }
            }
        }
    };
    let active_item_id = initial_task
        .as_ref()
        .and_then(|task| task.get("item_id"))
        .and_then(Value::as_str);
    let task_board_update = initial_task_board_snapshot(
        creation.work_id.as_str(),
        creation.branch_id.as_str(),
        work_goal,
        graph_revision,
        &initial_items,
        active_item_id,
    );
    ToolResult::text(
        json!({
            "status": "started",
            "activation": activation,
            "work_id": creation.work_id,
            "branch_id": creation.branch_id,
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
        return Err(ToolResult::error(
            "run_next_work_item requires a canonical Work bound to this session".to_string(),
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
        return Err(ToolResult::error(
            "run_next_work_item rejected because the session Work binding changed".to_string(),
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
        InitialRunnableItem, InitialWorkItem, InitialWorkTask, StartWorkArgs,
        TaskGraphExecutionStatus, board_settled_task, canonical_settlement_transition,
        compile_continuation_task_graph, compile_initial_task_graph, execute_start_work,
        initial_declared_tasks, initial_runnable_items, initial_task_board_snapshot,
        start_work_request_id, task_board_display_text, task_graph_execution_status,
        validate_initial_task_list,
    };
    use crate::server::runtime_tool_executor::ActivePrimaryWorkAttempt;
    use crate::server::runtime_tool_executor::RuntimeToolExecutor;
    use astra_services::work::{
        RecordedWorkAttemptSettlement, WorkAttemptOutcome, WorkItemAttemptId,
        WorkItemDeclarationState, WorkItemDelivery, WorkItemDeliveryStatus, WorkItemExecution,
        WorkItemExecutionStatus, WorkItemId, WorkItemKind, WorkItemRevision, WorkItemText,
        WorkTaskExecutionItem, WorkTaskExecutionNext,
    };
    use astra_tools::tool_engine::ToolInvocationMetadata;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn request_identity_is_exact_retry_stable_and_tool_call_scoped() {
        let first = start_work_request_id("run-1", "call-1");
        assert_eq!(first, start_work_request_id("run-1", "call-1"));
        assert_ne!(first, start_work_request_id("run-2", "call-1"));
        assert_ne!(first, start_work_request_id("run-1", "call-2"));
        assert!(first.starts_with("tool-"));
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
    fn continuation_task_ids_are_server_owned_and_collision_free() {
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
        let items = compile_continuation_task_graph(
            &[
                "root".to_string(),
                "task-1".to_string(),
                "task-3".to_string(),
            ],
            &tasks,
        );

        assert_eq!(
            items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-2", "task-4"]
        );
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

        let tasks = vec![InitialWorkItem {
            item_id: "task-1".to_string(),
            kind: "task",
            objective: "a".repeat(8 * 1024),
            expected_result: "b".repeat(8 * 1024),
        }];
        let receipt = initial_task_board_snapshot(
            "work-1",
            "main",
            "g".repeat(16 * 1024),
            1,
            &tasks,
            Some("task-1"),
        );
        let encoded = serde_json::to_vec(&receipt).expect("receipt serializes");
        assert!(
            encoded.len() < 4 * 1024,
            "live receipt must fit the event budget"
        );
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
}
