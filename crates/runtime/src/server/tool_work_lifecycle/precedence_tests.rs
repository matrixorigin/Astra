use super::{
    canonical_work_establishment_request, execute_run_next_work_item, execute_settle_work_item,
    execute_start_work, reconcile_admitted_graph_mutations,
};
use crate::server::runtime_tool_executor::{RuntimeToolExecutor, WorkRuntimeBinding};
use astra_core::SharedPool;
use astra_services::work::{
    DatabaseWorkAttemptSettlementService, DatabaseWorkEstablishmentService, DatabaseWorkRepository,
    InternalSessionId, NewWorkAttemptSettlement, PrimaryWorkAttemptAdvance,
    WorkAttemptExecutionMode, WorkAttemptOutcome, WorkAttemptSettlementError, WorkItemAttemptId,
    WorkItemDeclarationState, WorkItemExecutionStatus, WorkItemId, WorkItemRevision,
    WorkItemRevisionRef, WorkOwnerId, WorkRepository, compile_work_establishment_plan,
};
use astra_services::{WorkAdmissionDecision, WorkAdmissionGraphMutation, WorkAdmissionTask};
use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

use super::tests::{admit_running_test_session, database_work_executor, setup_pool};

fn task(objective: &str, expected_result: &str, after: &[usize]) -> WorkAdmissionTask {
    WorkAdmissionTask {
        objective: objective.to_string(),
        expected_result: expected_result.to_string(),
        after_initial_tasks: after.to_vec(),
    }
}

fn required_decision(
    goal: &str,
    tasks: Vec<WorkAdmissionTask>,
    mutations: Vec<WorkAdmissionGraphMutation>,
) -> WorkAdmissionDecision {
    WorkAdmissionDecision::Required {
        domain: None,
        workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
        mutation_completion_scope: astra_config::user_profile::MutationCompletionScope::Unknown,
        goal: goal.to_string(),
        tasks,
        deferred_graph_mutations: mutations,
        activation: astra_services::WorkAdmissionActivation::Start,
        execution_topology: astra_services::WorkExecutionTopology::Primary,
        required_capabilities: Vec::new(),
    }
}

fn start_args(goal: &str, tasks: &[WorkAdmissionTask]) -> Value {
    json!({
        "goal": goal,
        "activation": "start",
        "tasks": tasks,
    })
}

fn invocation<'a>(run_id: &'a str, call_id: &'a str) -> ToolInvocationMetadata<'a> {
    ToolInvocationMetadata {
        run_id: Some(run_id),
        turn_chain_id: Some("precedence-regression-replay"),
        tool_call_id: Some(call_id),
        admission_source: Some(ToolInvocationAdmissionSource::Policy),
        expected_control_epoch: None,
        task_resolution_authority: None,
    }
}

fn addition_item_id(operation_id: &str, addition_index: usize) -> String {
    let mut digest = Sha256::new();
    digest.update(b"continuation-work-items-v1\0");
    digest.update(format!("{operation_id}-mutations").as_bytes());
    let namespace = format!("{:x}", digest.finalize());
    format!("task-{}-{}", &namespace[..48], addition_index + 1)
}

async fn attach_fresh_executor(
    workspace: &std::path::Path,
    pool: SharedPool,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
) -> RuntimeToolExecutor {
    let repository = DatabaseWorkRepository::new(pool.clone());
    let binding = repository
        .load_session_plan_binding(owner, session)
        .await
        .expect("canonical session Work binding");
    let executor = database_work_executor(workspace, pool.clone(), owner, session);
    executor
        .install_work_binding(WorkRuntimeBinding::new(
            pool,
            owner.clone(),
            session.clone(),
            binding.work_id,
            binding.branch_id,
        ))
        .expect("attach fresh executor to canonical Work");
    executor
}

async fn establish(
    pool: &SharedPool,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
    run_id: &str,
    turn_id: &str,
    args: &Value,
    decision: &WorkAdmissionDecision,
    workspace: &std::path::Path,
) -> (
    RuntimeToolExecutor,
    astra_services::work::WorkEstablishmentRequest,
    Value,
) {
    let request =
        canonical_work_establishment_request(owner, session, turn_id, run_id, args, Some(decision))
            .expect("canonical establishment request");
    DatabaseWorkEstablishmentService::new(pool.clone())
        .admit(&request)
        .await
        .expect("durable establishment admission");
    let executor = database_work_executor(workspace, pool.clone(), owner, session);
    executor
        .bind_work_establishment_operation("physical-start", &request.operation_id)
        .expect("trusted establishment provenance");
    let result = execute_start_work(&executor, args, invocation(run_id, "physical-start")).await;
    assert!(!result.is_error, "start_work failed: {result:?}");
    let receipt = serde_json::from_str(&result.output).expect("structured start receipt");
    (executor, request, receipt)
}

fn successor_attempt_id(label: &str) -> WorkItemAttemptId {
    WorkItemAttemptId::parse(format!("{label}-{}", Uuid::new_v4())).expect("successor attempt")
}

fn delivered() -> NewWorkAttemptSettlement {
    NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Delivered,
        summary: "Predecessor evidence was delivered".to_string(),
        blocker_kind: None,
        unavailable_capabilities: Vec::new(),
    }
}

async fn terminal_cut_count(
    pool: &SharedPool,
    owner: &WorkOwnerId,
    work_id: &astra_services::work::WorkId,
    branch_id: &astra_services::work::WorkBranchId,
) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_terminal_cuts \
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(owner.as_str())
    .bind(work_id.as_str())
    .bind(branch_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("terminal cut count")
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn immediate_replace_inherits_predecessor_even_when_replacement_id_sorts_first() {
    let pool = setup_pool().await;
    let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
    let session = InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
    let run_id = format!("run-{}", Uuid::new_v4());
    crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
    admit_running_test_session(
        &pool,
        &owner,
        &session,
        &run_id,
        "Immediate replacement precedence",
    )
    .await;

    let goal = "Deliver the predecessor before its replacement successor";
    let initial = vec![
        task("Deliver predecessor", "Predecessor evidence", &[]),
        task("Original successor", "Original successor evidence", &[1]),
    ];
    let replacement = task("Replacement successor", "Replacement evidence", &[]);
    let decision = required_decision(
        goal,
        initial.clone(),
        vec![WorkAdmissionGraphMutation::Replace {
            target_initial_candidate: 2,
            target: initial[1].clone(),
            replacement: replacement.clone(),
            after_initial_tasks: Vec::new(),
        }],
    );
    let args = start_args(goal, &initial);

    // Reproduce the old failure mode instead of relying on a lucky hash: the
    // server-owned replacement identity must sort ahead of `task-1`.
    let mut selected = None;
    for seed in 0..256 {
        let turn_id = format!("precedence-seed-{seed}");
        let request = canonical_work_establishment_request(
            &owner,
            &session,
            &turn_id,
            &run_id,
            &args,
            Some(&decision),
        )
        .expect("candidate request");
        let item_id = addition_item_id(&request.operation_id, 0);
        if item_id.as_str() < "task-1" {
            selected = Some((turn_id, item_id));
            break;
        }
    }
    let (turn_id, replacement_id) = selected.expect("bounded seed finds a sorting regression ID");

    let temp = TempDir::new().expect("workspace");
    let (executor, request, receipt) = establish(
        &pool,
        &owner,
        &session,
        &run_id,
        &turn_id,
        &args,
        &decision,
        temp.path(),
    )
    .await;
    assert_eq!(receipt["initial_task"]["item_id"], "task-1");
    assert_eq!(
        execute_run_next_work_item(
            &executor,
            &json!({}),
            invocation(&run_id, "before-delivery")
        )
        .await
        .output
        .parse::<Value>()
        .expect("assignment receipt")["item_id"],
        "task-1",
        "the replacement cannot run before its inherited predecessor delivers"
    );

    let repository = DatabaseWorkRepository::new(pool.clone());
    let snapshot = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("mutated graph");
    assert!(snapshot.dependencies().iter().any(|edge| {
        edge.predecessor_item_id.as_str() == "task-1"
            && edge.successor_item_id.as_str() == replacement_id
    }));
    let bypass = DatabaseWorkAttemptSettlementService::new(pool.clone())
        .begin_attempt(astra_services::work::NewWorkItemAttempt {
            owner_id: owner.clone(),
            work_id: request.work_id.clone(),
            branch_id: request.branch_id.clone(),
            session_id: session.as_str().to_string(),
            item: WorkItemRevisionRef {
                item_id: WorkItemId::parse(replacement_id.clone()).expect("replacement item id"),
                revision: WorkItemRevision::INITIAL,
            },
            graph_revision: snapshot.basis().graph_revision,
            attempt_id: successor_attempt_id("dependency-bypass"),
            executor_run_id: run_id.clone(),
            execution_mode: WorkAttemptExecutionMode::Delegated,
        })
        .await;
    assert!(
        matches!(bypass, Err(WorkAttemptSettlementError::StaleAssignment)),
        "storage must reject direct execution of a dependency-blocked replacement: {bypass:?}"
    );
    assert_eq!(
        snapshot
            .items()
            .iter()
            .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
            .count(),
        1
    );

    let original_attempt = executor
        .active_primary_work_attempt()
        .expect("task-1 attempt");
    let mutation_replay_temp = TempDir::new().expect("mutation replay workspace");
    let mutation_replay =
        attach_fresh_executor(mutation_replay_temp.path(), pool.clone(), &owner, &session).await;
    let restored = execute_run_next_work_item(
        &mutation_replay,
        &json!({}),
        invocation(&run_id, "recover-after-immediate-mutation"),
    )
    .await;
    assert!(
        !restored.is_error,
        "recover task-1 assignment: {restored:?}"
    );
    let restored: Value = serde_json::from_str(&restored.output).expect("restored task-1");
    assert_eq!(restored["item_id"], "task-1");
    assert_eq!(restored["attempt_id"], original_attempt.attempt_id);
    let active = mutation_replay
        .active_primary_work_attempt()
        .expect("recovered task-1 attempt");
    let advanced = DatabaseWorkAttemptSettlementService::new(pool.clone())
        .record_and_advance_primary(
            owner.as_str(),
            &active.attempt_id,
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("immediate-replacement"),
        )
        .await
        .expect("deliver predecessor and advance");
    let replacement_attempt = match advanced.advance {
        PrimaryWorkAttemptAdvance::Assigned {
            attempt_id,
            item_id,
            ..
        } => {
            assert_eq!(item_id.as_str(), replacement_id);
            attempt_id
        }
        other => panic!("replacement must become ready after delivery: {other:?}"),
    };

    let recovery_temp = TempDir::new().expect("recovery workspace");
    let recovered =
        attach_fresh_executor(recovery_temp.path(), pool.clone(), &owner, &session).await;
    let replay = execute_run_next_work_item(
        &recovered,
        &json!({}),
        invocation(&run_id, "recover-immediate-replacement"),
    )
    .await;
    assert!(!replay.is_error, "recover committed assignment: {replay:?}");
    let replay: Value = serde_json::from_str(&replay.output).expect("recovery assignment");
    assert_eq!(replay["item_id"], replacement_id);
    assert_eq!(replay["attempt_id"], replacement_attempt.as_str());
    let recovered_snapshot = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("recovered graph");
    assert_eq!(
        recovered_snapshot
            .items()
            .iter()
            .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
            .count(),
        1,
        "fresh recovery must not duplicate the committed assignment"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn delayed_cancel_and_add_apply_only_after_delivered_and_recover_once() {
    let pool = setup_pool().await;
    let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
    let session = InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
    let run_id = format!("run-{}", Uuid::new_v4());
    crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
    admit_running_test_session(
        &pool,
        &owner,
        &session,
        &run_id,
        "Delayed mutation precedence",
    )
    .await;

    let goal = "Deliver task one before cancelling task two and adding its successor";
    let initial = vec![
        task("Deliver mutation trigger", "Trigger evidence", &[]),
        task("Cancel after trigger", "Cancelled declaration", &[]),
    ];
    let addition = task("Added after trigger", "Added evidence", &[]);
    let decision = required_decision(
        goal,
        initial.clone(),
        vec![
            WorkAdmissionGraphMutation::Cancel {
                target_initial_candidate: 2,
                target: initial[1].clone(),
                after_initial_tasks: vec![1],
            },
            WorkAdmissionGraphMutation::Add {
                task: addition.clone(),
                after_initial_tasks: vec![1],
            },
        ],
    );
    let args = start_args(goal, &initial);
    let turn_id = "delayed-cancel-add";
    let expected_request = canonical_work_establishment_request(
        &owner,
        &session,
        turn_id,
        &run_id,
        &args,
        Some(&decision),
    )
    .expect("expected request");
    let addition_id = addition_item_id(&expected_request.operation_id, 0);

    let temp = TempDir::new().expect("workspace");
    let (executor, _, receipt) = establish(
        &pool,
        &owner,
        &session,
        &run_id,
        turn_id,
        &args,
        &decision,
        temp.path(),
    )
    .await;
    assert_eq!(receipt["initial_task"]["item_id"], "task-1");
    let repository = DatabaseWorkRepository::new(pool.clone());
    let before = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("graph before trigger delivery");
    assert!(
        before.items().iter().any(|item| {
            item.item_id.as_str() == "task-2"
                && item.declaration_state == WorkItemDeclarationState::Active
        }),
        "cancel must remain pending before task-1 delivers"
    );
    assert!(
        !before
            .items()
            .iter()
            .any(|item| item.item_id.as_str() == addition_id)
    );
    assert!(
        before.pending_graph_mutations().is_empty(),
        "the trigger has not delivered, so no mutation group is due"
    );

    // Fault window: settlement is durable, but the runtime has not yet
    // reconciled the newly eligible mutation group.
    let active = executor
        .active_primary_work_attempt()
        .expect("task-1 attempt");
    let advanced = DatabaseWorkAttemptSettlementService::new(pool.clone())
        .record_and_advance_primary(
            owner.as_str(),
            &active.attempt_id,
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("must-not-be-task-2"),
        )
        .await
        .expect("durable task-1 settlement");
    assert_eq!(
        advanced.advance,
        PrimaryWorkAttemptAdvance::GraphMutationPending
    );
    let settled = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("settled pre-reconciliation graph");
    assert_eq!(
        settled
            .items()
            .iter()
            .find(|item| item.item_id.as_str() == "task-1")
            .expect("task-1")
            .delivery
            .status,
        astra_services::work::WorkItemDeliveryStatus::Delivered
    );
    assert!(
        !settled
            .items()
            .iter()
            .any(|item| item.item_id.as_str() == addition_id)
    );

    let reconciliation_temp = TempDir::new().expect("reconciliation workspace");
    let reconciliation_executor =
        attach_fresh_executor(reconciliation_temp.path(), pool.clone(), &owner, &session).await;
    assert!(
        reconcile_admitted_graph_mutations(
            &reconciliation_executor,
            invocation(&run_id, "commit-delayed-mutations"),
        )
        .await
        .expect("commit due delayed mutation group")
        .is_some(),
        "the delivered trigger must make its mutation group eligible"
    );
    assert!(
        !reconciliation_executor.has_active_primary_work_attempt(),
        "mutation reconciliation itself must not create an assignment"
    );

    // The acceptance graph revision and the exact trigger association are
    // durable facts, so a fresh reader can explain the change before any
    // successor assignment or receipt replay occurs.
    let applied = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("applied mutation snapshot");
    assert_eq!(applied.applied_graph_mutations().len(), 1);
    let applied_mutation = &applied.applied_graph_mutations()[0];
    assert_eq!(
        applied_mutation
            .trigger_attempt_id
            .as_ref()
            .map(WorkItemAttemptId::as_str),
        Some(active.attempt_id.as_str())
    );
    assert_eq!(
        applied_mutation.trigger_item,
        Some(WorkItemRevisionRef {
            item_id: WorkItemId::parse("task-1").expect("trigger item"),
            revision: WorkItemRevision::INITIAL,
        })
    );
    assert!(applied_mutation.trigger_association_known);

    // Queue retention is independent of the canonical acceptance fact. A
    // terminal proposal may be pruned without making an already-applied
    // graph mutation pending again.
    let proposal_id = applied_mutation
        .group
        .proposal_id(
            owner.as_str(),
            session.as_str(),
            applied.basis().work_id.as_str(),
            applied.basis().branch_id.as_str(),
        )
        .expect("mutation proposal identity");
    sqlx::query(
        "DELETE FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(applied.basis().work_id.as_str())
    .bind(applied.basis().branch_id.as_str())
    .bind(proposal_id.as_str())
    .execute(pool.get())
    .await
    .expect("prune terminal proposal");
    let pruned = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("snapshot after proposal pruning");
    assert!(pruned.pending_graph_mutations().is_empty());
    assert_eq!(pruned.applied_graph_mutations().len(), 1);

    // A pre-association accepted revision is still applied, but its historic
    // trigger cannot be reconstructed. Never infer that it belongs to the
    // current attempt.
    sqlx::query(
        "DELETE FROM work_proposal_trigger_attempts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(applied.basis().work_id.as_str())
    .bind(applied.basis().branch_id.as_str())
    .bind(proposal_id.as_str())
    .execute(pool.get())
    .await
    .expect("remove trigger association for legacy recovery");
    let legacy = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("legacy accepted snapshot");
    assert_eq!(legacy.applied_graph_mutations().len(), 1);
    assert!(!legacy.applied_graph_mutations()[0].trigger_association_known);
    let active_attempt_id =
        WorkItemAttemptId::parse(active.attempt_id.clone()).expect("active attempt identity");
    let (legacy_receipt, attribution_available) = super::applied_admission_mutations_for_attempt(
        &legacy,
        &active_attempt_id,
        &WorkItemRevisionRef {
            item_id: WorkItemId::parse("task-1").expect("trigger item"),
            revision: WorkItemRevision::INITIAL,
        },
    );
    assert!(!attribution_available);
    assert_eq!(legacy_receipt.len(), 1);

    // Crash after the mutation proposal commits but before assignment. A
    // fresh executor must recover from that exact durable boundary.
    let recovery_temp = TempDir::new().expect("recovery workspace");
    let recovered =
        attach_fresh_executor(recovery_temp.path(), pool.clone(), &owner, &session).await;
    let assigned = execute_run_next_work_item(
        &recovered,
        &json!({}),
        invocation(&run_id, "reconcile-delayed-mutations"),
    )
    .await;
    assert!(
        !assigned.is_error,
        "reconcile and assign addition: {assigned:?}"
    );
    let assigned: Value = serde_json::from_str(&assigned.output).expect("addition assignment");
    assert_eq!(assigned["item_id"], addition_id);
    let assigned_attempt = assigned["attempt_id"]
        .as_str()
        .expect("attempt id")
        .to_string();
    assert_eq!(
        assigned["applied_admission_mutations"][0]["added_item_ids"],
        json!([addition_id.clone()])
    );
    assert_eq!(
        assigned["applied_admission_mutations"][0]["revised_items"][0],
        json!({
            "item_id": "task-2",
            "from_revision": 1,
            "declaration_state": "cancelled"
        })
    );

    let committed = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("committed delayed mutations");
    assert!(committed.pending_graph_mutations().is_empty());
    assert_eq!(
        committed
            .items()
            .iter()
            .find(|item| item.item_id.as_str() == "task-2")
            .expect("cancelled task-2")
            .declaration_state,
        WorkItemDeclarationState::Cancelled
    );
    assert_eq!(
        committed
            .items()
            .iter()
            .filter(|item| item.execution.status == WorkItemExecutionStatus::Running)
            .count(),
        1
    );

    let replay_temp = TempDir::new().expect("second recovery workspace");
    let replay_executor =
        attach_fresh_executor(replay_temp.path(), pool.clone(), &owner, &session).await;
    let replay = execute_run_next_work_item(
        &replay_executor,
        &json!({}),
        invocation(&run_id, "replay-delayed-mutations"),
    )
    .await;
    assert!(
        !replay.is_error,
        "recover committed delayed mutation: {replay:?}"
    );
    let replay: Value = serde_json::from_str(&replay.output).expect("replayed assignment");
    assert_eq!(replay["item_id"], addition_id);
    assert_eq!(replay["attempt_id"], assigned_attempt);
    let replayed = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("replayed graph");
    assert_eq!(replayed.items().len(), committed.items().len());
    assert_eq!(
        replayed
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
async fn trigger_marker_conflict_rolls_back_settlement_and_delivery() {
    let pool = setup_pool().await;
    let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
    let session = InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
    let run_id = format!("run-{}", Uuid::new_v4());
    crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
    admit_running_test_session(
        &pool,
        &owner,
        &session,
        &run_id,
        "Deferred marker transaction rollback",
    )
    .await;

    let goal = "Keep trigger settlement atomic with its deferred mutation receipt";
    let initial = vec![
        task("Deliver transaction trigger", "Trigger evidence", &[]),
        task("Remain available", "Available evidence", &[]),
    ];
    let decision = required_decision(
        goal,
        initial.clone(),
        vec![WorkAdmissionGraphMutation::Add {
            task: task("Add after transaction trigger", "Added evidence", &[]),
            after_initial_tasks: vec![1],
        }],
    );
    let args = start_args(goal, &initial);
    let turn_id = "deferred-marker-transaction";
    let workspace = TempDir::new().expect("workspace");
    let (executor, request, _) = establish(
        &pool,
        &owner,
        &session,
        &run_id,
        turn_id,
        &args,
        &decision,
        workspace.path(),
    )
    .await;
    let active = executor
        .active_primary_work_attempt()
        .expect("trigger attempt");
    let plan = compile_work_establishment_plan(
        &request.operation_id,
        &initial,
        decision.deferred_graph_mutations(),
    )
    .expect("compiled deferred mutation");
    let group = &plan.mutation_groups[0];
    let proposal_id = group
        .proposal_id(
            owner.as_str(),
            session.as_str(),
            request.work_id.as_str(),
            request.branch_id.as_str(),
        )
        .expect("mutation proposal identity");
    let conflicting_attempt = format!("conflicting-attempt-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO work_proposal_trigger_attempts
         (owner_id, work_id, branch_id, proposal_id, trigger_attempt_id,
          trigger_item_id, trigger_item_revision)
         VALUES (?, ?, ?, ?, ?, 'task-1', 1)",
    )
    .bind(owner.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(proposal_id.as_str())
    .bind(&conflicting_attempt)
    .execute(pool.get())
    .await
    .expect("seed conflicting trigger marker");

    let result = DatabaseWorkAttemptSettlementService::new(pool.clone())
        .record_and_advance_primary(
            owner.as_str(),
            &active.attempt_id,
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("marker-conflict-successor"),
        )
        .await;
    assert!(
        matches!(&result, Err(WorkAttemptSettlementError::Persistence(_))),
        "marker conflict must abort the settlement transaction: {result:?}"
    );

    let outcome: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM work_item_attempts
         WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(owner.as_str())
    .bind(active.attempt_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("settlement outcome after rollback");
    assert!(
        outcome.is_none(),
        "delivery must roll back with the conflicting marker"
    );
    let marker_attempt: String = sqlx::query_scalar(
        "SELECT trigger_attempt_id FROM work_proposal_trigger_attempts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(proposal_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("conflicting marker remains authoritative");
    assert_eq!(marker_attempt, conflicting_attempt);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn multi_trigger_mutation_records_only_the_final_settlement() {
    let pool = setup_pool().await;
    let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
    let session = InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
    let run_id = format!("run-{}", Uuid::new_v4());
    crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
    admit_running_test_session(
        &pool,
        &owner,
        &session,
        &run_id,
        "Multiple deferred mutation triggers",
    )
    .await;

    let goal = "Wait for both trigger tasks before applying one deferred cancellation";
    let initial = vec![
        task("Deliver first trigger", "First trigger evidence", &[]),
        task("Deliver final trigger", "Final trigger evidence", &[]),
        task("Cancel after both triggers", "Cancelled declaration", &[]),
    ];
    let decision = required_decision(
        goal,
        initial.clone(),
        vec![WorkAdmissionGraphMutation::Cancel {
            target_initial_candidate: 3,
            target: initial[2].clone(),
            after_initial_tasks: vec![1, 2],
        }],
    );
    let args = start_args(goal, &initial);
    let workspace = TempDir::new().expect("workspace");
    let (executor, request, _) = establish(
        &pool,
        &owner,
        &session,
        &run_id,
        "multiple-deferred-triggers",
        &args,
        &decision,
        workspace.path(),
    )
    .await;
    let first = executor
        .active_primary_work_attempt()
        .expect("first trigger attempt");
    let second_attempt = successor_attempt_id("second-trigger");
    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    let first_advance = service
        .record_and_advance_primary(
            owner.as_str(),
            &first.attempt_id,
            &run_id,
            -1,
            delivered(),
            second_attempt.clone(),
        )
        .await
        .expect("settle first trigger");
    assert!(
        matches!(
            first_advance.advance,
            PrimaryWorkAttemptAdvance::Assigned { .. }
        ),
        "the second trigger must be assigned before the group is due: {:?}",
        first_advance.advance
    );

    let second_advance = service
        .record_and_advance_primary(
            owner.as_str(),
            second_attempt.as_str(),
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("after-two-triggers"),
        )
        .await
        .expect("settle final trigger");
    assert_eq!(
        second_advance.advance,
        PrimaryWorkAttemptAdvance::GraphMutationPending,
        "the mutation must wait for explicit reconciliation"
    );

    let repository = DatabaseWorkRepository::new(pool.clone());
    let pending = repository
        .load_task_execution_snapshot_for_session(&owner, &session)
        .await
        .expect("pending multi-trigger graph");
    assert_eq!(pending.pending_graph_mutations().len(), 1);
    let proposal_id = pending.pending_graph_mutations()[0]
        .proposal_id(
            owner.as_str(),
            session.as_str(),
            request.work_id.as_str(),
            request.branch_id.as_str(),
        )
        .expect("multi-trigger proposal identity");
    let marker_attempt: Option<String> = sqlx::query_scalar(
        "SELECT trigger_attempt_id FROM work_proposal_trigger_attempts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(proposal_id.as_str())
    .fetch_optional(pool.get())
    .await
    .expect("load final trigger marker");
    assert_eq!(marker_attempt.as_deref(), Some(second_attempt.as_str()));

    // Replaying the first trigger while the group is still pending must not
    // overwrite the final trigger association.
    let replay_first = service
        .record_and_advance_primary(
            owner.as_str(),
            &first.attempt_id,
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("replay-first-trigger"),
        )
        .await
        .expect("replay first trigger");
    assert_eq!(
        replay_first.advance,
        PrimaryWorkAttemptAdvance::GraphMutationPending
    );
    let marker_after_first_replay: String = sqlx::query_scalar(
        "SELECT trigger_attempt_id FROM work_proposal_trigger_attempts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(proposal_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("marker after first replay");
    assert_eq!(marker_after_first_replay, second_attempt.as_str());

    reconcile_admitted_graph_mutations(&executor, invocation(&run_id, "apply-two-trigger"))
        .await
        .expect("apply multi-trigger mutation")
        .expect("one applied mutation");
    let replay_second = service
        .record_and_advance_primary(
            owner.as_str(),
            second_attempt.as_str(),
            &run_id,
            -1,
            delivered(),
            successor_attempt_id("replay-final-trigger"),
        )
        .await
        .expect("replay final trigger");
    assert_eq!(
        replay_second.advance,
        PrimaryWorkAttemptAdvance::Complete,
        "the final trigger replay may finish the canonical graph"
    );
    let marker_after_second_replay: String = sqlx::query_scalar(
        "SELECT trigger_attempt_id FROM work_proposal_trigger_attempts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
    )
    .bind(owner.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(proposal_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("marker after final replay");
    assert_eq!(marker_after_second_replay, second_attempt.as_str());
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn delayed_cancel_terminal_cut_recovers_across_both_commit_windows() {
    let pool = setup_pool().await;
    for recovery_window in 0..5 {
        let case = match recovery_window {
            0 => "settlement-committed-before-mutation",
            1 => "mutation-committed-before-terminal-cut",
            2 => "settlement-committed-before-receipt-read-failure",
            3 => "fresh-executor-before-terminal-cut",
            _ => "proposal-pruned-before-terminal-cut",
        };
        let owner = WorkOwnerId::parse(format!("owner-{}", Uuid::new_v4())).expect("owner");
        let session =
            InternalSessionId::parse(format!("session-{}", Uuid::new_v4())).expect("session");
        let run_id = format!("run-{}", Uuid::new_v4());
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
        admit_running_test_session(&pool, &owner, &session, &run_id, case).await;

        let goal = "Deliver task one before cancelling the remaining declaration";
        let initial = vec![
            task("Deliver terminal trigger", "Terminal trigger evidence", &[]),
            task(
                "Cancel after terminal trigger",
                "Cancelled declaration",
                &[],
            ),
        ];
        let decision = required_decision(
            goal,
            initial.clone(),
            vec![WorkAdmissionGraphMutation::Cancel {
                target_initial_candidate: 2,
                target: initial[1].clone(),
                after_initial_tasks: vec![1],
            }],
        );
        let args = start_args(goal, &initial);
        let temp = TempDir::new().expect("workspace");
        let turn_id = format!("delayed-cancel-{case}");
        let (executor, request, receipt) = establish(
            &pool,
            &owner,
            &session,
            &run_id,
            &turn_id,
            &args,
            &decision,
            temp.path(),
        )
        .await;
        assert_eq!(receipt["initial_task"]["item_id"], "task-1", "{case}");
        let active = executor
            .active_primary_work_attempt()
            .expect("task-1 attempt");
        let settlement = delivered();

        let precommit = DatabaseWorkAttemptSettlementService::new(pool.clone())
            .record_and_advance_primary(
                owner.as_str(),
                &active.attempt_id,
                &run_id,
                -1,
                settlement.clone(),
                successor_attempt_id("precommit-mutation-pending"),
            )
            .await
            .expect("precommit exact task delivery");
        assert_eq!(
            precommit.advance,
            PrimaryWorkAttemptAdvance::GraphMutationPending,
            "{case} must persist the trigger association before reconciliation"
        );
        let repository = DatabaseWorkRepository::new(pool.clone());
        let pre_replay = repository
            .load_task_execution_snapshot_for_session(&owner, &session)
            .await
            .expect("pre-replay snapshot");
        assert_eq!(pre_replay.pending_graph_mutations().len(), 1, "{case}");
        assert_eq!(
            terminal_cut_count(&pool, &owner, &request.work_id, &request.branch_id).await,
            0,
            "{case}"
        );

        if recovery_window != 0 {
            assert!(
                reconcile_admitted_graph_mutations(
                    &executor,
                    invocation(&run_id, "commit-cancel-before-terminal-cut"),
                )
                .await
                .expect("commit delayed cancellation")
                .is_some()
            );
            assert_eq!(
                terminal_cut_count(&pool, &owner, &request.work_id, &request.branch_id).await,
                0,
                "mutation commit must not impersonate terminal settlement"
            );
        }
        if recovery_window == 4 {
            let graph = repository
                .load_task_execution_snapshot_for_session(&owner, &session)
                .await
                .expect("accepted graph before proposal pruning");
            let proposal_id = graph.applied_graph_mutations()[0]
                .group
                .proposal_id(
                    owner.as_str(),
                    session.as_str(),
                    request.work_id.as_str(),
                    request.branch_id.as_str(),
                )
                .expect("accepted mutation proposal identity");
            sqlx::query(
                "DELETE FROM work_proposals
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND proposal_id = ?",
            )
            .bind(owner.as_str())
            .bind(request.work_id.as_str())
            .bind(request.branch_id.as_str())
            .bind(proposal_id.as_str())
            .execute(pool.get())
            .await
            .expect("prune accepted proposal before terminal recovery");
        }
        if recovery_window == 3 {
            let repair_temp = TempDir::new().expect("terminal-cut repair workspace");
            let repair =
                attach_fresh_executor(repair_temp.path(), pool.clone(), &owner, &session).await;
            let repaired = execute_run_next_work_item(
                &repair,
                &json!({}),
                ToolInvocationMetadata {
                    run_id: Some(&run_id),
                    turn_chain_id: Some("terminal-cut-repair-turn"),
                    tool_call_id: Some("terminal-cut-repair-call"),
                    admission_source: Some(ToolInvocationAdmissionSource::Policy),
                    expected_control_epoch: Some(-1),
                    task_resolution_authority: None,
                },
            )
            .await;
            assert!(!repaired.is_error, "repair terminal cut: {repaired:?}");
            let repaired: Value = serde_json::from_str(&repaired.output).expect("repair receipt");
            assert_eq!(repaired["status"], "complete");
            let board = &repaired["task_board_update"];
            assert!(board["graph_revision"].as_i64().is_some(), "{board}");
            assert!(
                board["tasks"]
                    .as_array()
                    .expect("repair board")
                    .iter()
                    .any(|task| {
                        task["item_id"] == "task-2" && task["declaration_state"] == "cancelled"
                    }),
                "{board}"
            );
        } else if recovery_window == 2 {
            // Enter the exact recovery path used when the post-commit board
            // read fails. Durable settlement/allocation must not be repeated
            // merely to recover its user-visible receipt.
            DatabaseWorkAttemptSettlementService::new(pool.clone())
                .record_and_advance_primary(
                    owner.as_str(),
                    &active.attempt_id,
                    &run_id,
                    7,
                    settlement.clone(),
                    WorkItemAttemptId::parse(format!("receipt-recovery-{}", Uuid::new_v4()))
                        .expect("successor identity"),
                )
                .await
                .expect("commit settlement before receipt read failure");
            let pending = super::committed_settlement_resume_error(
                &executor,
                &active,
                "injected post-commit board read failure".to_string(),
            );
            assert!(pending.is_error);
            assert!(executor.active_primary_work_attempt().is_none());
            let resumed = execute_run_next_work_item(
                &executor,
                &json!({}),
                invocation(&run_id, "resume-after-receipt-read-failure"),
            )
            .await;
            assert!(!resumed.is_error, "{resumed:?}");
            let resumed: Value = serde_json::from_str(&resumed.output).expect("resume receipt");
            assert_eq!(resumed["status"], "complete");
            let board = &resumed["task_board_update"];
            assert!(board["graph_revision"].as_i64().is_some(), "{board}");
            assert!(
                board["tasks"]
                    .as_array()
                    .expect("resume board")
                    .iter()
                    .any(|task| {
                        task["item_id"] == "task-2" && task["declaration_state"] == "cancelled"
                    }),
                "{board}"
            );
        } else {
            let settled = execute_settle_work_item(
                &executor,
                &serde_json::to_value(&settlement).expect("settlement arguments"),
                ToolInvocationMetadata {
                    run_id: Some(&run_id),
                    turn_chain_id: Some("settlement-replay-turn"),
                    tool_call_id: Some("settlement-replay-call"),
                    admission_source: Some(ToolInvocationAdmissionSource::Policy),
                    expected_control_epoch: Some(7),
                    task_resolution_authority: None,
                },
            )
            .await;
            assert!(
                !settled.is_error,
                "handler must reconcile and replay committed settlement: {settled:?}"
            );
            let settled: Value = serde_json::from_str(&settled.output).expect("settlement receipt");
            assert_eq!(settled["execution_status"], "complete");
            assert_eq!(settled["next_action"], "synthesize_final_response");
            let board = &settled["task_board_update"];
            assert!(board["graph_revision"].as_i64().is_some());
            assert!(
                board["tasks"]
                    .as_array()
                    .expect("canonical board tasks")
                    .iter()
                    .any(|task| {
                        task["item_id"] == "task-2" && task["declaration_state"] == "cancelled"
                    }),
                "settlement must publish the cancelled declaration: {board}"
            );
        }

        let completed = repository
            .load_task_execution_snapshot_for_session(&owner, &session)
            .await
            .expect("completed snapshot");
        assert!(completed.pending_graph_mutations().is_empty());
        assert_eq!(
            completed
                .items()
                .iter()
                .find(|item| item.item_id.as_str() == "task-2")
                .expect("cancelled task")
                .declaration_state,
            WorkItemDeclarationState::Cancelled
        );
        assert_eq!(
            terminal_cut_count(&pool, &owner, &request.work_id, &request.branch_id).await,
            1,
            "{case} must publish exactly one terminal cut"
        );

        let recovery_temp = TempDir::new().expect("terminal recovery workspace");
        let recovered =
            attach_fresh_executor(recovery_temp.path(), pool.clone(), &owner, &session).await;
        let next = execute_run_next_work_item(
            &recovered,
            &json!({}),
            invocation(&run_id, "recover-terminal-cut"),
        )
        .await;
        assert!(!next.is_error, "recover terminal graph ({case}): {next:?}");
        let next: Value = serde_json::from_str(&next.output).expect("terminal recovery receipt");
        assert_eq!(next["status"], "complete");
        let board = &next["task_board_update"];
        assert!(board["graph_revision"].as_i64().is_some(), "{board}");
        assert!(
            board["tasks"]
                .as_array()
                .expect("recovered board")
                .iter()
                .any(|task| {
                    task["item_id"] == "task-2" && task["declaration_state"] == "cancelled"
                }),
            "fresh recovery must publish cancelled declarations ({case}): {board}"
        );
        assert_eq!(
            terminal_cut_count(&pool, &owner, &request.work_id, &request.branch_id).await,
            1,
            "fresh recovery must not duplicate the terminal cut ({case})"
        );
    }
}
