mod common;

use astra_services::work::{
    DatabaseWorkRepository, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkBranchSubjectChange, WorkBranchSubjectInvalidation, WorkBranchSubjectRevision,
    WorkChangeRef, WorkContentHash, WorkGenesis, WorkId, WorkOwnerId, WorkRepository,
    WorkRepositoryError, WorkSubjectRef,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn hash(byte: char) -> WorkContentHash {
    WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &id("session"),
        &id("intent"),
        "Deliver and verify one exact materialized target.",
    )
}

fn subject_change(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    expected_branch_revision: WorkBranchRevision,
    subject_revision: WorkContentHash,
    source_ref: &str,
) -> WorkBranchSubjectChange {
    WorkBranchSubjectChange {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        expected_branch_revision,
        graph_revision: GraphRevision::INITIAL,
        subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head").expect("subject"),
        subject_revision,
        source_ref: WorkChangeRef::parse(source_ref).expect("source"),
    }
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("work_runtime_event_outbox", "owner_id"),
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_current_gap_acceptances", "owner_id"),
        ("work_acceptance_decisions", "owner_id"),
        ("work_check_runs", "owner_id"),
        ("work_proposals", "owner_id"),
        ("work_proposal_sequences", "owner_id"),
        ("work_branch_subjects", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_criteria", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_subject_cas_has_one_winner_and_exact_replays_are_noops() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    assert_eq!(
        repository
            .load_branch_subject(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await
            .expect("subject lookup"),
        None
    );
    assert!(matches!(
        repository
            .load_branch_subject(
                &WorkOwnerId::parse(&other_owner_id).expect("other owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    let left_source = id("materialization-left");
    let right_source = id("materialization-right");
    let left_change = subject_change(
        &owner_id,
        &work_id,
        &branch_id,
        WorkBranchRevision::INITIAL,
        hash('a'),
        &left_source,
    );
    let right_change = subject_change(
        &owner_id,
        &work_id,
        &branch_id,
        WorkBranchRevision::INITIAL,
        hash('b'),
        &right_source,
    );
    let (left, right) = tokio::join!(
        repository.set_branch_subject(left_change.clone()),
        repository.set_branch_subject(right_change.clone())
    );
    let winner = match (left, right) {
        (Ok(winner), Err(WorkRepositoryError::StaleSubjectBasis { .. }))
        | (Err(WorkRepositoryError::StaleSubjectBasis { .. }), Ok(winner)) => winner,
        results => panic!("exactly one subject CAS must win: {results:?}"),
    };
    assert_eq!(
        winner.subject_record_revision,
        WorkBranchSubjectRevision::INITIAL
    );
    assert_eq!(
        winner.branch_revision,
        WorkBranchRevision::new(2).expect("branch r2")
    );

    let winning_change = if winner.subject_revision == hash('a') {
        left_change
    } else {
        right_change
    };
    let replay = repository
        .set_branch_subject(winning_change.clone())
        .await
        .expect("an exact stale-CAS replay is idempotent");
    assert_eq!(replay, winner);
    let no_op = repository
        .set_branch_subject(WorkBranchSubjectChange {
            expected_branch_revision: winner.branch_revision,
            source_ref: WorkChangeRef::parse(id("same-target-new-observation")).expect("source"),
            ..winning_change
        })
        .await
        .expect("same immutable target is a semantic no-op");
    assert_eq!(no_op, winner);

    let invalidation = WorkBranchSubjectInvalidation {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
        expected_branch_revision: winner.branch_revision,
        graph_revision: GraphRevision::INITIAL,
        source_ref: WorkChangeRef::parse(id("execution-boundary")).expect("source"),
    };
    let invalidated_revision = repository
        .invalidate_branch_subject(invalidation.clone())
        .await
        .expect("invalidate exact subject before execution");
    assert_eq!(
        invalidated_revision,
        WorkBranchRevision::new(3).expect("branch r3")
    );
    assert_eq!(
        repository
            .invalidate_branch_subject(invalidation)
            .await
            .expect("invalidation replay is a semantic no-op"),
        invalidated_revision
    );
    assert!(
        repository
            .load_branch_subject(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await
            .expect("subject lookup")
            .is_none()
    );

    let branch_revision: i64 = sqlx::query(
        "SELECT branch_revision FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("branch")
    .try_get("branch_revision")
    .expect("revision");
    assert_eq!(branch_revision, 3, "replays must not churn branch revision");
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("events");
    assert_eq!(
        events,
        ["work_created", "subject_changed", "subject_changed"]
    );

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn event_identity_failure_rolls_back_subject_and_branch_together() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let source_ref = id("materialization");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let original = repository
        .set_branch_subject(subject_change(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            hash('a'),
            &source_ref,
        ))
        .await
        .expect("first subject");

    let conflicting_event_identity = subject_change(
        &owner_id,
        &work_id,
        &branch_id,
        original.branch_revision,
        hash('b'),
        &source_ref,
    );
    assert!(matches!(
        repository
            .set_branch_subject(conflicting_event_identity)
            .await,
        Err(WorkRepositoryError::Conflict { .. })
    ));
    let retained = repository
        .load_branch_subject(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
            &WorkBranchId::parse(&branch_id).expect("branch"),
        )
        .await
        .expect("load subject")
        .expect("subject");
    assert_eq!(retained, original, "subject mutation must roll back");
    let branch_revision: i64 = sqlx::query_scalar(
        "SELECT branch_revision FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("branch revision");
    assert_eq!(branch_revision, 2, "branch CAS must roll back with subject");
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_events WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(&work_id)
            .fetch_one(pool.get())
            .await
            .expect("event count");
    assert_eq!(event_count, 2, "failed subject change leaves no event");

    cleanup_owner(&pool, &owner_id).await;
}
