mod common;

use astra_services::work::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionSetRevision, CriterionStatement,
    DatabaseWorkRepository, GoalRevision, GraphRevision, NewWorkCriteriaProposal, WorkBranchId,
    WorkBranchRevision, WorkChangeRef, WorkCriteriaProposalAcceptance, WorkCriteriaProposalMember,
    WorkCriteriaProposalRejection, WorkGoal, WorkGoalChange, WorkId, WorkOwnerId, WorkProposalId,
    WorkProposalSourceKind, WorkProposalStatus, WorkRepository, WorkRepositoryError, WorkRevision,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> astra_services::work::WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &id("session"),
        &id("intent"),
        "Deliver the feature with explicit evidence.",
    )
}

fn new_test_criterion(criterion_id: &str, statement: &str) -> WorkCriteriaProposalMember {
    WorkCriteriaProposalMember::New {
        criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
        definition: CriterionDefinition::TestCheck {
            statement: CriterionStatement::parse(statement).expect("statement"),
            command: CriterionCommand::parse("cargo test -p astra-services").expect("command"),
        },
    }
}

fn proposal(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    proposal_id: &str,
    members: Vec<WorkCriteriaProposalMember>,
) -> NewWorkCriteriaProposal {
    NewWorkCriteriaProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        proposal_id: WorkProposalId::parse(proposal_id).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        members,
        source_kind: WorkProposalSourceKind::Model,
        source_ref: WorkChangeRef::parse(id("model-invocation")).expect("source"),
    }
}

fn acceptance(
    proposed: &astra_services::work::RecordedWorkCriteriaProposal,
    resolution_ref: &str,
) -> WorkCriteriaProposalAcceptance {
    WorkCriteriaProposalAcceptance {
        owner_id: proposed.proposal.owner_id.clone(),
        work_id: proposed.proposal.work_id.clone(),
        branch_id: proposed.proposal.branch_id.clone(),
        proposal_id: proposed.proposal.proposal_id.clone(),
        payload_hash: proposed.payload_hash.clone(),
        expected_work_revision: proposed.proposal.expected_work_revision,
        expected_goal_revision: proposed.proposal.expected_goal_revision,
        expected_criteria_set_revision: proposed.proposal.expected_criteria_set_revision,
        expected_branch_revision: proposed.proposal.expected_branch_revision,
        expected_graph_revision: proposed.proposal.expected_graph_revision,
        resolution_ref: WorkChangeRef::parse(resolution_ref).expect("resolution"),
    }
}

fn rejection(
    proposed: &astra_services::work::RecordedWorkCriteriaProposal,
    resolution_ref: &str,
) -> WorkCriteriaProposalRejection {
    WorkCriteriaProposalRejection {
        owner_id: proposed.proposal.owner_id.clone(),
        work_id: proposed.proposal.work_id.clone(),
        branch_id: proposed.proposal.branch_id.clone(),
        proposal_id: proposed.proposal.proposal_id.clone(),
        payload_hash: proposed.payload_hash.clone(),
        expected_work_revision: proposed.proposal.expected_work_revision,
        expected_goal_revision: proposed.proposal.expected_goal_revision,
        expected_criteria_set_revision: proposed.proposal.expected_criteria_set_revision,
        expected_branch_revision: proposed.proposal.expected_branch_revision,
        expected_graph_revision: proposed.proposal.expected_graph_revision,
        resolution_ref: WorkChangeRef::parse(resolution_ref).expect("resolution"),
    }
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for table in [
        "work_events",
        "work_attention_receipts",
        "work_event_sequences",
        "work_proposals",
        "work_proposal_sequences",
        "work_acceptance_decisions",
        "work_check_runs",
        "work_branch_subjects",
        "work_item_edges",
        "work_item_revisions",
        "work_items",
        "work_graph_revisions",
        "work_graph_sequences",
        "work_branches",
        "work_criterion_sets",
        "work_criterion_revisions",
        "work_criteria",
        "work_goal_revisions",
        "works",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE owner_id = ?"))
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn criteria_proposal_is_canonical_bounded_discoverable_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let concurrent = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let proposal_id = id("proposal");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let input = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &proposal_id,
        vec![
            new_test_criterion(&id("criterion-b"), "The integration contract passes."),
            new_test_criterion(&id("criterion-a"), "The domain contract passes."),
        ],
    );
    let (left, right) = tokio::join!(
        repository.propose_criteria(input.clone()),
        concurrent.propose_criteria(input.clone())
    );
    let left = left.expect("first proposal");
    let right = right.expect("idempotent concurrent proposal");
    assert_eq!(left, right);
    assert_eq!(left.status, WorkProposalStatus::Pending);
    let member_ids = left
        .proposal
        .members
        .iter()
        .map(|member| match member {
            WorkCriteriaProposalMember::Existing { criterion_id, .. }
            | WorkCriteriaProposalMember::New { criterion_id, .. } => criterion_id.as_str(),
        })
        .collect::<Vec<_>>();
    assert!(member_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        repository
            .list_pending_criteria_proposals(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await
            .expect("pending list"),
        vec![left.clone()]
    );
    assert!(
        repository
            .load_criteria_proposal(
                &WorkOwnerId::parse(&other_owner_id).expect("other owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkProposalId::parse(&proposal_id).expect("proposal"),
            )
            .await
            .expect("owner-isolated read")
            .is_none()
    );
    let mut conflicting = input;
    conflicting.members = vec![new_test_criterion(
        &id("different"),
        "A different typed criterion passes.",
    )];
    assert!(matches!(
        repository.propose_criteria(conflicting).await,
        Err(WorkRepositoryError::Conflict { .. })
    ));
    let counts = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?) AS proposals,
            (SELECT COUNT(*) FROM work_events WHERE owner_id = ? AND work_id = ?
              AND event_kind = 'criteria_proposed') AS events",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("counts");
    assert_eq!(counts.try_get::<i64, _>("proposals").unwrap(), 1);
    assert_eq!(counts.try_get::<i64, _>("events").unwrap(), 1);
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_acceptance_is_atomic_idempotent_and_keeps_branch_basis_explicit() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let concurrent = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &id("proposal"),
            vec![new_test_criterion(
                &criterion_id,
                "The targeted repository test passes.",
            )],
        ))
        .await
        .expect("proposal");
    let command = acceptance(&proposed, &id("accept-action"));
    let (left, right) = tokio::join!(
        repository.accept_criteria_proposal(command.clone()),
        concurrent.accept_criteria_proposal(command)
    );
    let left = left.expect("first acceptance");
    let right = right.expect("exact retry");
    assert_eq!(left, right);
    assert_eq!(left.status, WorkProposalStatus::Accepted);
    let resolution = left.resolution.expect("accepted resolution");
    assert_eq!(
        resolution.result_work_revision,
        Some(WorkRevision::new(2).expect("Work r2"))
    );
    assert_eq!(
        resolution.result_criteria_set_revision,
        Some(CriterionSetRevision::new(2).expect("criteria r2"))
    );
    let state = sqlx::query(
        "SELECT w.work_revision, w.current_criteria_set_revision,
                b.branch_revision, b.criteria_set_revision_ref,
                (SELECT COUNT(*) FROM work_criterion_revisions cr
                  WHERE cr.owner_id = w.owner_id AND cr.work_id = w.work_id) AS definitions,
                (SELECT COUNT(*) FROM work_events e
                  WHERE e.owner_id = w.owner_id AND e.work_id = w.work_id
                    AND e.event_kind = 'criteria_accepted') AS accepted_events
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
          AND b.branch_id = ?
         WHERE w.owner_id = ? AND w.work_id = ?",
    )
    .bind(&branch_id)
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("state");
    assert_eq!(state.try_get::<i64, _>("work_revision").unwrap(), 2);
    assert_eq!(
        state
            .try_get::<i64, _>("current_criteria_set_revision")
            .unwrap(),
        2
    );
    assert_eq!(state.try_get::<i64, _>("branch_revision").unwrap(), 1);
    assert_eq!(
        state
            .try_get::<i64, _>("criteria_set_revision_ref")
            .unwrap(),
        1,
        "accepting Work-level criteria must not silently rewrite branch basis"
    );
    assert_eq!(state.try_get::<i64, _>("definitions").unwrap(), 1);
    assert_eq!(state.try_get::<i64, _>("accepted_events").unwrap(), 1);
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn stale_acceptance_has_no_residue_but_exact_rejection_remains_available() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &id("proposal"),
            vec![new_test_criterion(
                &id("criterion"),
                "The stale proposal must not materialize.",
            )],
        ))
        .await
        .expect("proposal");
    repository
        .revise_goal(WorkGoalChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            goal: WorkGoal::parse("Deliver the revised goal with explicit evidence.")
                .expect("goal"),
            source_ref: WorkChangeRef::parse(id("goal-action")).expect("source"),
            reason: None,
        })
        .await
        .expect("revise Goal");
    assert!(matches!(
        repository
            .accept_criteria_proposal(acceptance(&proposed, &id("stale-accept")))
            .await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis { .. })
    ));
    let residue = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM work_criterion_revisions WHERE owner_id = ? AND work_id = ?) AS definitions,
            (SELECT COUNT(*) FROM work_criterion_sets WHERE owner_id = ? AND work_id = ?) AS sets",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("residue");
    assert_eq!(residue.try_get::<i64, _>("definitions").unwrap(), 0);
    assert_eq!(residue.try_get::<i64, _>("sets").unwrap(), 1);

    let rejected = repository
        .reject_criteria_proposal(rejection(&proposed, &id("reject-action")))
        .await
        .expect("reject stale proposal explicitly");
    assert_eq!(rejected.status, WorkProposalStatus::Rejected);
    assert_eq!(
        repository
            .reject_criteria_proposal(WorkCriteriaProposalRejection {
                resolution_ref: rejected
                    .resolution
                    .as_ref()
                    .expect("resolution")
                    .resolution_ref
                    .clone(),
                ..rejection(&proposed, "unused-resolution")
            })
            .await
            .expect("exact rejection retry"),
        rejected
    );
    assert!(
        repository
            .list_pending_criteria_proposals(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await
            .expect("pending list")
            .is_empty()
    );
    let event_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'proposal_rejected'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("rejection event")
    .try_get::<i64, _>("count")
    .unwrap();
    assert_eq!(event_count, 1);
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn acceptance_event_conflict_rolls_back_work_criteria_and_proposal() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let proposal_id = id("proposal");
    let resolution_ref = id("accept-action");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &proposal_id,
            vec![new_test_criterion(
                &id("criterion"),
                "The transaction either commits completely or not at all.",
            )],
        ))
        .await
        .expect("proposal");
    sqlx::query(
        "INSERT INTO work_events
         (owner_id, work_id, event_seq, event_kind, work_revision,
          criterion_set_revision, source_ref)
         VALUES (?, ?, 9000, 'criteria_accepted', 2, 2, ?)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&resolution_ref)
    .execute(pool.get())
    .await
    .expect("inject criteria event identity conflict");
    assert!(matches!(
        repository
            .accept_criteria_proposal(acceptance(&proposed, &resolution_ref))
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: astra_services::work::WorkConflictResource::WorkEventIdentity
        })
    ));
    let state = sqlx::query(
        "SELECT w.work_revision, w.current_criteria_set_revision,
                p.status, p.resolution_ref,
                (SELECT COUNT(*) FROM work_criterion_sets cs
                  WHERE cs.owner_id = w.owner_id AND cs.work_id = w.work_id) AS sets,
                (SELECT COUNT(*) FROM work_criterion_revisions cr
                  WHERE cr.owner_id = w.owner_id AND cr.work_id = w.work_id) AS definitions,
                es.last_event_seq
         FROM works w
         JOIN work_proposals p
           ON p.owner_id = w.owner_id AND p.work_id = w.work_id AND p.proposal_id = ?
         JOIN work_event_sequences es
           ON es.owner_id = w.owner_id AND es.work_id = w.work_id
         WHERE w.owner_id = ? AND w.work_id = ?",
    )
    .bind(&proposal_id)
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("rolled back state");
    assert_eq!(state.try_get::<i64, _>("work_revision").unwrap(), 1);
    assert_eq!(
        state
            .try_get::<i64, _>("current_criteria_set_revision")
            .unwrap(),
        1
    );
    assert_eq!(state.try_get::<String, _>("status").unwrap(), "pending");
    assert_eq!(
        state
            .try_get::<Option<String>, _>("resolution_ref")
            .unwrap(),
        None
    );
    assert_eq!(state.try_get::<i64, _>("sets").unwrap(), 1);
    assert_eq!(state.try_get::<i64, _>("definitions").unwrap(), 0);
    assert_eq!(state.try_get::<i64, _>("last_event_seq").unwrap(), 2);
    cleanup_owner(&pool, &owner_id).await;
}
