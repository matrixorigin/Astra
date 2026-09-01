mod common;

use astra_services::work::{
    CheckCoverage, CheckEvidenceRef, CheckOutcome, CheckRunId, CheckVerifierKind, CriterionCommand,
    CriterionDefinition, CriterionId, CriterionRevision, CriterionRevisionRef,
    CriterionSetMemberChange, CriterionSetRevision, DatabaseWorkRepository, GraphRevision,
    NewWorkCheckRun, NewWorkCriterion, WorkBranchBasisChange, WorkBranchId, WorkBranchRevision,
    WorkBranchSubjectChange, WorkChangeRef, WorkContentHash, WorkCriteriaChange, WorkGenesis,
    WorkGraphChange, WorkId, WorkItemAttemptId, WorkItemId, WorkItemRevision, WorkItemRevisionRef,
    WorkOwnerId, WorkRepository, WorkRepositoryError, WorkRevision, WorkSubjectRef,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn hash(byte: char) -> WorkContentHash {
    WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
}

fn attempt_id(branch_id: &str) -> String {
    format!("run-{branch_id}")
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &id("session"),
        &id("intent"),
        "Prove a revision-bound verifier result.",
    )
}

async fn accept_test_criterion(
    repository: &DatabaseWorkRepository,
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    criterion_id: &str,
) {
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: astra_services::work::CriterionStatement::parse(
                        "The targeted verifier passes on the exact subject revision.",
                    )
                    .expect("statement"),
                    command: CriterionCommand::parse("cargo test -p example targeted_test")
                        .expect("command"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("accept criterion");
    repository
        .adopt_branch_basis(WorkBranchBasisChange {
            owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_work_revision: WorkRevision::new(2).expect("work r2"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            target_goal_revision: astra_services::work::GoalRevision::INITIAL,
            target_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            source_ref: WorkChangeRef::parse(id("basis-source")).expect("source"),
        })
        .await
        .expect("adopt criterion set");
}

fn check(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    criterion_id: &str,
    check_run_id: &str,
) -> NewWorkCheckRun {
    NewWorkCheckRun {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        check_run_id: CheckRunId::parse(check_run_id).expect("check run"),
        graph_revision: GraphRevision::INITIAL,
        item: WorkItemRevisionRef {
            item_id: WorkItemId::root(),
            revision: WorkItemRevision::INITIAL,
        },
        attempt_id: WorkItemAttemptId::parse(attempt_id(branch_id)).expect("attempt"),
        criterion_set_revision: CriterionSetRevision::new(2).expect("criterion set r2"),
        criterion: CriterionRevisionRef {
            criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
            revision: CriterionRevision::INITIAL,
        },
        subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
        subject_revision: hash('a'),
        artifact_digest: Some(hash('b')),
        run_ref: WorkChangeRef::parse(attempt_id(branch_id)).expect("run"),
        invocation_ref: WorkChangeRef::parse(id("invocation")).expect("invocation"),
        verifier_kind: CheckVerifierKind::Test,
        verifier_fingerprint: hash('c'),
        environment_fingerprint: hash('d'),
        outcome: CheckOutcome::Passed,
        error_kind: None,
        coverage: CheckCoverage::Complete,
        coverage_gaps: Vec::new(),
        evidence_refs: vec![
            CheckEvidenceRef::parse("urn:astra:trace:cloud:check-1/invocation")
                .expect("trace evidence"),
            CheckEvidenceRef::parse("urn:astra:artifact:cloud:check-1/result")
                .expect("artifact evidence"),
        ],
        source_cursor: WorkChangeRef::parse(id("cursor")).expect("cursor"),
        produced_at: "2026-08-01T00:00:00Z".parse().expect("time"),
        expires_at: None,
    }
}

async fn establish_subject(
    pool: &astra_core::SharedPool,
    repository: &DatabaseWorkRepository,
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
) {
    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(2).expect("branch r2"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
            subject_revision: hash('a'),
            source_ref: WorkChangeRef::parse(id("subject-source")).expect("source"),
        })
        .await
        .expect("establish current subject");
    let run_id = attempt_id(branch_id);
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status,
          work_id, work_branch_id, work_graph_revision,
          work_item_id, work_item_revision, work_item_attempt_id)
         VALUES (?, ?, ?, ?, ?, 0, 'completed', ?, ?, 1, 'root', 1, ?)",
    )
    .bind(&run_id)
    .bind(owner_id)
    .bind(id("run-session"))
    .bind(&run_id)
    .bind(&run_id)
    .bind(work_id)
    .bind(branch_id)
    .bind(&run_id)
    .execute(pool.get())
    .await
    .expect("persist exact root WorkItem attempt");
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("agent_runs", "user_id"),
        ("work_runtime_event_outbox", "owner_id"),
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
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

async fn check_count(pool: &astra_core::SharedPool, owner_id: &str, work_id: &str) -> i64 {
    sqlx::query("SELECT COUNT(*) AS count FROM work_check_runs WHERE owner_id = ? AND work_id = ?")
        .bind(owner_id)
        .bind(work_id)
        .fetch_one(pool.get())
        .await
        .expect("count checks")
        .try_get("count")
        .expect("count")
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn check_admission_requires_an_explicit_revision_pinned_branch_basis() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: astra_services::work::CriterionStatement::parse(
                        "The exact adopted branch basis is verified.",
                    )
                    .expect("statement"),
                    command: CriterionCommand::parse("verify exact adopted basis")
                        .expect("command"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("accept criteria");
    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            graph_revision: GraphRevision::INITIAL,
            subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
            subject_revision: hash('a'),
            source_ref: WorkChangeRef::parse(id("subject-source")).expect("source"),
        })
        .await
        .expect("establish subject before adoption");
    let run_id = attempt_id(&branch_id);
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status,
          work_id, work_branch_id, work_graph_revision,
          work_item_id, work_item_revision, work_item_attempt_id)
         VALUES (?, ?, ?, ?, ?, 0, 'completed', ?, ?, 1, 'root', 1, ?)",
    )
    .bind(&run_id)
    .bind(&owner_id)
    .bind(id("run-session"))
    .bind(&run_id)
    .bind(&run_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&run_id)
    .execute(pool.get())
    .await
    .expect("persist exact run");

    let candidate = check(&owner_id, &work_id, &branch_id, &criterion_id, &id("check"));
    assert!(matches!(
        repository.record_check_run(candidate.clone()).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::CriterionSetRevision
        })
    ));
    let adoption = WorkBranchBasisChange {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
        expected_work_revision: WorkRevision::new(2).expect("work r2"),
        expected_branch_revision: WorkBranchRevision::new(2).expect("branch r2"),
        expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        target_goal_revision: astra_services::work::GoalRevision::INITIAL,
        target_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
        source_ref: WorkChangeRef::parse(id("basis-source")).expect("source"),
    };
    let (left, right) = tokio::join!(
        repository.adopt_branch_basis(adoption.clone()),
        repository.adopt_branch_basis(adoption.clone())
    );
    let adopted = left.expect("first exact adoption");
    assert_eq!(right.expect("concurrent exact adoption"), adopted);
    assert_eq!(
        adopted.parts().branch_revision,
        WorkBranchRevision::new(3).expect("branch r3")
    );
    let replay = repository
        .adopt_branch_basis(adoption)
        .await
        .expect("exact adoption retry");
    assert_eq!(replay, adopted);
    repository
        .record_check_run(candidate)
        .await
        .expect("admitted after exact adoption");
    assert_eq!(check_count(&pool, &owner_id, &work_id).await, 1);
    let adoption_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'branch_basis_adopted'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("count adoption events");
    assert_eq!(adoption_events, 1);

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_check_retry_is_idempotent_and_canonical() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    let check_run_id = id("check");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_test_criterion(&repository, &owner_id, &work_id, &branch_id, &criterion_id).await;
    let no_subject = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("no-subject-check"),
    );
    assert!(matches!(
        repository.record_check_run(no_subject).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::Subject
        })
    ));
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;

    let first = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &check_run_id,
    );
    let mut semantic_retry = first.clone();
    semantic_retry.evidence_refs.reverse();
    let (left, right) = tokio::join!(
        repository.record_check_run(first),
        repository.record_check_run(semantic_retry)
    );
    let left = left.expect("first check");
    let right = right.expect("idempotent retry");
    assert_eq!(left.payload_hash, right.payload_hash);
    assert_eq!(left.created_at, right.created_at);
    assert_eq!(check_count(&pool, &owner_id, &work_id).await, 1);

    let distinct_ids = [id("check-a"), id("check-b"), id("check-c"), id("check-d")];
    let (a, b, c, d) = tokio::join!(
        repository.record_check_run(check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &distinct_ids[0],
        )),
        repository.record_check_run(check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &distinct_ids[1],
        )),
        repository.record_check_run(check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &distinct_ids[2],
        )),
        repository.record_check_run(check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &distinct_ids[3],
        )),
    );
    for result in [a, b, c, d] {
        result.expect("independent check identity");
    }
    assert_eq!(
        check_count(&pool, &owner_id, &work_id).await,
        5,
        "independent verifier results must not share one per-Work writer lease"
    );

    let row = sqlx::query(
        "SELECT CAST(evidence_refs_json AS CHAR) AS evidence_refs_json,
                criterion_definition_hash, payload_hash,
                work_item_id, work_item_revision, work_item_attempt_id
         FROM work_check_runs
         WHERE owner_id = ? AND work_id = ? AND check_run_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&check_run_id)
    .fetch_one(pool.get())
    .await
    .expect("check row");
    let evidence: Vec<String> = serde_json::from_str(
        &row.try_get::<String, _>("evidence_refs_json")
            .expect("evidence JSON"),
    )
    .expect("evidence refs");
    assert_eq!(evidence.len(), 2);
    assert!(
        evidence[0] < evidence[1],
        "set-like evidence must be canonical"
    );
    assert_eq!(
        row.try_get::<String, _>("criterion_definition_hash")
            .expect("definition hash")
            .len(),
        71
    );
    assert_eq!(
        row.try_get::<String, _>("payload_hash")
            .expect("payload hash")
            .len(),
        71
    );
    assert_eq!(
        row.try_get::<String, _>("work_item_id").expect("item"),
        "root"
    );
    assert_eq!(
        row.try_get::<i64, _>("work_item_revision")
            .expect("item revision"),
        1
    );
    assert_eq!(
        row.try_get::<String, _>("work_item_attempt_id")
            .expect("attempt"),
        attempt_id(&branch_id)
    );

    let mut conflicting_retry = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &check_run_id,
    );
    conflicting_retry.outcome = CheckOutcome::Failed;
    let conflict = repository
        .record_check_run(conflicting_retry)
        .await
        .expect_err("same identity cannot change payload");
    assert!(matches!(
        conflict,
        WorkRepositoryError::Conflict {
            resource: astra_services::work::WorkConflictResource::CheckRunIdentity
        }
    ));
    assert_eq!(check_count(&pool, &owner_id, &work_id).await, 5);
    let events = sqlx::query(
        "SELECT event_seq, event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("Work events");
    assert_eq!(
        events.len(),
        9,
        "one semantic event per committed check, never per retry"
    );
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.try_get::<i64, _>("event_seq").expect("sequence"),
            i64::try_from(index + 1).expect("sequence")
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event
                .try_get::<String, _>("event_kind")
                .is_ok_and(|kind| kind == "check_recorded"))
            .count(),
        5
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn wrong_owner_or_unaccepted_basis_cannot_leave_check_rows() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_test_criterion(&repository, &owner_id, &work_id, &branch_id, &criterion_id).await;
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;

    let mut wrong_owner = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("wrong-owner-check"),
    );
    wrong_owner.owner_id = WorkOwnerId::parse(&other_owner_id).expect("other owner");
    assert!(matches!(
        repository.record_check_run(wrong_owner).await,
        Err(WorkRepositoryError::NotFound)
    ));

    let mut unaccepted_basis = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("unaccepted-check"),
    );
    unaccepted_basis.criterion_set_revision = CriterionSetRevision::INITIAL;
    assert!(matches!(
        repository.record_check_run(unaccepted_basis).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::CriterionSetRevision
        })
    ));

    let mut wrong_verifier = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("wrong-verifier-check"),
    );
    wrong_verifier.verifier_kind = CheckVerifierKind::Command;
    assert!(matches!(
        repository.record_check_run(wrong_verifier).await,
        Err(WorkRepositoryError::CheckVerifierMismatch {
            criterion_kind: "test_check",
            verifier_kind: "command"
        })
    ));

    let mut wrong_attempt = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("wrong-attempt-check"),
    );
    wrong_attempt.attempt_id = WorkItemAttemptId::parse("another-attempt").expect("attempt");
    assert!(matches!(
        repository.record_check_run(wrong_attempt).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::RunBinding
        })
    ));

    let mut unknown_item = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("unknown-item-check"),
    );
    unknown_item.item.item_id = WorkItemId::parse("not-in-graph").expect("item");
    assert!(matches!(
        repository.record_check_run(unknown_item).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::WorkItemRevision
        })
    ));

    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(3).expect("branch r3"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
            subject_revision: hash('e'),
            source_ref: WorkChangeRef::parse(id("new-subject-source")).expect("source"),
        })
        .await
        .expect("advance subject");
    let stale_subject = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("stale-subject-check"),
    );
    assert!(matches!(
        repository.record_check_run(stale_subject).await,
        Err(WorkRepositoryError::InvalidCheckBasis {
            resource: astra_services::work::WorkCheckBasisResource::Subject
        })
    ));

    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(4).expect("branch r4"),
            expected_graph_revision: GraphRevision::INITIAL,
            items: Vec::new(),
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("advance graph");
    let stale = check(
        &owner_id,
        &work_id,
        &branch_id,
        &criterion_id,
        &id("stale-check"),
    );
    assert!(matches!(
        repository.record_check_run(stale).await,
        Err(WorkRepositoryError::StaleCheckGraphRevision {
            evidence_graph_revision,
            current_graph_revision
        }) if evidence_graph_revision == GraphRevision::INITIAL
            && current_graph_revision == GraphRevision::new(2).expect("graph r2")
    ));
    assert_eq!(check_count(&pool, &owner_id, &work_id).await, 0);
    let event_rows = sqlx::query(
        "SELECT event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("Work events");
    let event_kinds = event_rows
        .iter()
        .map(|row| row.try_get::<String, _>("event_kind").expect("kind"))
        .collect::<Vec<_>>();
    assert_eq!(
        event_kinds,
        [
            "work_created",
            "criteria_accepted",
            "branch_basis_adopted",
            "subject_changed",
            "subject_changed",
            "graph_replaced"
        ],
        "the graph mutation commits once while rejected checks leave no events"
    );

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn corrupt_retention_source_rolls_back_the_new_check_and_event_sequence() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    let old_check_id = id("old-check");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_test_criterion(&repository, &owner_id, &work_id, &branch_id, &criterion_id).await;
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;
    repository
        .record_check_run(check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &old_check_id,
        ))
        .await
        .expect("old check");
    sqlx::query(
        "DELETE FROM work_check_runs
         WHERE owner_id = ? AND work_id = ? AND check_run_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&old_check_id)
    .execute(pool.get())
    .await
    .expect("simulate corrupt missing check detail");
    sqlx::query(
        "UPDATE work_event_sequences
         SET last_event_seq = 10004, retained_from_event_seq = 5
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("place check event at retention boundary");

    let new_check_id = id("new-check");
    assert!(matches!(
        repository
            .record_check_run(check(
                &owner_id,
                &work_id,
                &branch_id,
                &criterion_id,
                &new_check_id,
            ))
            .await,
        Err(WorkRepositoryError::Corrupt { .. })
    ));
    assert_eq!(
        check_count(&pool, &owner_id, &work_id).await,
        0,
        "the new check insert must roll back with retention failure"
    );
    let sequence = sqlx::query(
        "SELECT last_event_seq, retained_from_event_seq FROM work_event_sequences
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("event sequence");
    assert_eq!(
        sequence.try_get::<i64, _>("last_event_seq").expect("head"),
        10004
    );
    assert_eq!(
        sequence
            .try_get::<i64, _>("retained_from_event_seq")
            .expect("floor"),
        5
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM work_events
             WHERE owner_id = ? AND work_id = ? AND event_seq = 10005",
        )
        .bind(&owner_id)
        .bind(&work_id)
        .fetch_one(pool.get())
        .await
        .expect("new event count"),
        0
    );

    cleanup_owner(&pool, &owner_id).await;
}
