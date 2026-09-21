mod common;

use astra_services::work::{
    DatabaseWorkRepository, GoalRevision, GraphRevision, NewWorkItem, NewWorkPlanProposal,
    WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkGoal, WorkGoalChange, WorkId,
    WorkItemEdge, WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemText, WorkOwnerId,
    WorkProposalId, WorkProposalSourceKind, WorkProposalStatus, WorkRepository,
    WorkRepositoryError, WorkRevision,
};
use sqlx::Row;

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> astra_services::work::WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &common::id("session"),
        &common::id("intent"),
        "Expand a typed plan without changing authority.",
    )
}

fn item(item_id: &str) -> NewWorkItem {
    NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item"),
        kind: WorkItemKind::Task,
        objective: WorkItemText::parse(format!("Implement {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} has exact verification"))
            .expect("expected result"),
    }
}

fn dependency(predecessor: &str, successor: &str) -> WorkItemEdge {
    WorkItemEdge {
        predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
        successor_item_id: WorkItemId::parse(successor).expect("successor"),
        kind: WorkItemEdgeKind::Dependency,
    }
}

fn proposal(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    proposal_id: &str,
    additions: Vec<NewWorkItem>,
    dependencies: Vec<WorkItemEdge>,
) -> NewWorkPlanProposal {
    NewWorkPlanProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        proposal_id: WorkProposalId::parse(proposal_id).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: astra_services::work::CriterionSetRevision::INITIAL,
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        additions,
        revisions: Vec::new(),
        dependencies,
        dependency_removals: Vec::new(),
        reason: astra_services::work::WorkChangeReason::parse("Refine the Work plan")
            .expect("reason"),
        source_kind: WorkProposalSourceKind::Model,
        source_ref: WorkChangeRef::parse(common::id("model-invocation")).expect("source"),
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn proposal_is_canonical_idempotent_non_authoritative_and_revision_pinned() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let other_owner_id = common::id("other-owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let proposal_id = common::id("proposal");
    let task_a = common::id("task-a");
    let task_b = common::id("task-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let first = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &proposal_id,
        vec![item(&task_b), item(&task_a)],
        vec![dependency(&task_a, &task_b)],
    );
    let mut reordered = first.clone();
    reordered.additions.reverse();
    let (left, right) = tokio::join!(
        repository.propose_plan(first.clone()),
        repository.propose_plan(reordered)
    );
    let left = left.expect("first proposal");
    let right = right.expect("canonical retry");
    assert_eq!(left, right);
    assert_eq!(left.proposal_seq, 1);
    assert_eq!(left.status, WorkProposalStatus::Pending);
    assert_eq!(
        repository
            .load_plan_proposal(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkProposalId::parse(&proposal_id).expect("proposal"),
            )
            .await
            .expect("load exact proposal"),
        Some(left.clone())
    );
    assert_eq!(
        repository
            .load_plan_proposal(
                &WorkOwnerId::parse(&other_owner_id).expect("other owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkProposalId::parse(&proposal_id).expect("proposal"),
            )
            .await
            .expect("owner-isolated proposal lookup"),
        None
    );

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("Work remains readable");
    assert_eq!(
        loaded.delivery_branch.parts().branch_revision,
        WorkBranchRevision::INITIAL,
        "a pending model proposal must not mutate graph authority"
    );
    assert_eq!(
        loaded.delivery_branch.parts().current_graph_revision,
        GraphRevision::INITIAL
    );
    let sequence = sqlx::query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal sequence");
    assert_eq!(sequence.try_get::<i64, _>("last_proposal_seq").unwrap(), 1);

    let canonical_payload: String = sqlx::query_scalar(
        "SELECT payload_json FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .fetch_one(pool.get())
    .await
    .expect("canonical payload");
    sqlx::query(
        "UPDATE work_proposals SET payload_json = '{\"corrupt\":true}'
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("inject payload corruption");
    assert!(matches!(
        repository.propose_plan(first.clone()).await,
        Err(WorkRepositoryError::Corrupt {
            entity: "plan proposal payload",
            ..
        })
    ));
    sqlx::query(
        "UPDATE work_proposals SET payload_json = ?
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(canonical_payload)
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("restore canonical payload");
    sqlx::query(
        "UPDATE work_proposals SET expected_graph_revision = 2
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("inject row summary corruption");
    assert!(matches!(
        repository.propose_plan(first.clone()).await,
        Err(WorkRepositoryError::Corrupt {
            entity: "plan proposal",
            ..
        })
    ));
    sqlx::query(
        "UPDATE work_proposals SET expected_graph_revision = 1
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("restore row summary");

    let mut conflicting = first.clone();
    conflicting.additions[0].expected_result =
        WorkItemText::parse("A different result").expect("result");
    assert!(matches!(
        repository.propose_plan(conflicting).await,
        Err(WorkRepositoryError::Conflict { .. })
    ));
    let mut wrong_owner = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("wrong-owner-proposal"),
        vec![item(&common::id("other-task"))],
        Vec::new(),
    );
    wrong_owner.owner_id = WorkOwnerId::parse(&other_owner_id).expect("other owner");
    assert!(matches!(
        repository.propose_plan(wrong_owner).await,
        Err(WorkRepositoryError::NotFound)
    ));

    let missing_endpoint = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("missing-endpoint"),
        vec![item(&common::id("valid-task"))],
        vec![dependency(
            &common::id("missing"),
            &common::id("also-missing"),
        )],
    );
    assert!(matches!(
        repository.propose_plan(missing_endpoint).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::DependencyEndpoint
        })
    ));
    let cyclic = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("cyclic"),
        vec![item(&task_a), item(&task_b)],
        vec![dependency(&task_a, &task_b), dependency(&task_b, &task_a)],
    );
    assert!(matches!(
        repository.propose_plan(cyclic).await,
        Err(WorkRepositoryError::InvalidMutation {
            source: astra_services::work::WorkDomainError::CyclicWorkItemGraph
        })
    ));

    let revised = repository
        .revise_goal(WorkGoalChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            goal: WorkGoal::parse("A new goal makes delayed proposals stale.").expect("goal"),
            source_ref: WorkChangeRef::parse(common::id("goal-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("revise goal");
    let mut incoherent = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("incoherent-proposal"),
        vec![item(&common::id("incoherent-task"))],
        Vec::new(),
    );
    incoherent.expected_work_revision = revised.work.parts().work_revision;
    incoherent.expected_goal_revision = revised.work.parts().current_goal_revision;
    assert!(matches!(
        repository.propose_plan(incoherent).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::BranchGoalRevision
        })
    ));
    let stale = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("stale-proposal"),
        vec![item(&common::id("stale-task"))],
        Vec::new(),
    );
    assert!(matches!(
        repository.propose_plan(stale).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::WorkRevision
        })
    ));
    let event_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("events");
    assert_eq!(
        event_kinds,
        ["work_created", "plan_proposed", "goal_revised"]
    );

    common::cleanup_work_owner(&pool, &owner_id).await;
    common::cleanup_work_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn pending_capacity_is_bounded_and_expiry_reclaims_one_slot() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let mut proposal_ids = Vec::new();
    for index in 0..8 {
        let proposal_id = common::id(&format!("proposal-{index}"));
        proposal_ids.push(proposal_id.clone());
        repository
            .propose_plan(proposal(
                &owner_id,
                &work_id,
                &branch_id,
                &proposal_id,
                vec![item(&common::id(&format!("task-{index}")))],
                Vec::new(),
            ))
            .await
            .expect("bounded pending proposal");
    }
    let ninth_id = common::id("proposal-nine");
    let ninth = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &ninth_id,
        vec![item(&common::id("task-nine"))],
        Vec::new(),
    );
    assert!(matches!(
        repository.propose_plan(ninth.clone()).await,
        Err(WorkRepositoryError::WorkProposalCapacityExceeded)
    ));
    sqlx::query(
        "UPDATE work_proposals
         SET proposed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY),
             expires_at = DATE_SUB(NOW(6), INTERVAL 1 DAY)
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_ids[0])
    .execute(pool.get())
    .await
    .expect("expire oldest pending proposal");
    let admitted = repository
        .propose_plan(ninth)
        .await
        .expect("expired proposal frees capacity");
    assert_eq!(admitted.proposal_seq, 9);
    let sequence = sqlx::query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal sequence");
    assert_eq!(sequence.try_get::<i64, _>("last_proposal_seq").unwrap(), 9);
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND status = 'pending'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("pending proposal count");
    assert_eq!(pending_count, 8);
    let expired_status: String = sqlx::query_scalar(
        "SELECT status FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_ids[0])
    .fetch_one(pool.get())
    .await
    .expect("expired status");
    assert_eq!(expired_status, "expired");
    let proposed_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'plan_proposed'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal event count");
    assert_eq!(proposed_events, 9, "capacity rejection emits no event");

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn proposal_event_conflict_rolls_back_capacity_and_payload() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let proposal_id = common::id("proposal");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    sqlx::query(
        "INSERT INTO work_events
         (owner_id, work_id, event_seq, branch_id, event_kind, work_revision,
          goal_revision, criterion_set_revision, branch_revision, graph_revision, source_ref)
         VALUES (?, ?, 9000, ?, 'plan_proposed', 1, 1, 1, 1, 1, ?)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("inject conflicting event identity");

    assert!(matches!(
        repository
            .propose_plan(proposal(
                &owner_id,
                &work_id,
                &branch_id,
                &proposal_id,
                vec![item(&common::id("task"))],
                Vec::new(),
            ))
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: astra_services::work::WorkConflictResource::WorkEventIdentity
        })
    ));
    let sequence = sqlx::query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal sequence");
    assert_eq!(sequence.try_get::<i64, _>("last_proposal_seq").unwrap(), 0);
    let proposal_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal count");
    assert_eq!(proposal_count, 0);
    let event_head: i64 = sqlx::query_scalar(
        "SELECT last_event_seq FROM work_event_sequences WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("event head");
    assert_eq!(event_head, 1, "event allocation must roll back");

    common::cleanup_work_owner(&pool, &owner_id).await;
}
