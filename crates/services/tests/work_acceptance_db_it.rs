mod common;

use astra_services::work::{
    AcceptanceDecisionId, AcceptanceGapReason, AcceptedCriterionGap, CheckCoverage,
    CheckCoverageGap, CheckEvidenceRef, CheckOutcome, CheckRunId, CheckVerifierKind,
    CriterionCommand, CriterionDefinition, CriterionId, CriterionRevision, CriterionRevisionRef,
    CriterionSetMemberChange, CriterionSetRevision, DatabaseWorkRepository, GoalRevision,
    GraphRevision, NewWorkAcceptanceDecision, NewWorkCheckRun, NewWorkCriterion,
    WorkBranchBasisChange, WorkBranchId, WorkBranchRevision, WorkBranchSubjectChange,
    WorkChangeRef, WorkContentHash, WorkCriteriaChange, WorkGenesis, WorkGoal, WorkGoalChange,
    WorkId, WorkItemAttemptId, WorkItemId, WorkItemRevision, WorkItemRevisionRef, WorkOwnerId,
    WorkRepository, WorkRepositoryError, WorkRevision, WorkSubjectRef,
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
        "Deliver with an explicit evidence-gap decision.",
    )
}

fn criterion_ref(criterion_id: &str) -> CriterionRevisionRef {
    CriterionRevisionRef {
        criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
        revision: CriterionRevision::INITIAL,
    }
}

async fn accept_criteria(
    repository: &DatabaseWorkRepository,
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    criterion_ids: &[String],
) {
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: criterion_ids
                .iter()
                .map(|criterion_id| {
                    CriterionSetMemberChange::New(NewWorkCriterion {
                        criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
                        definition: CriterionDefinition::TestCheck {
                            statement: astra_services::work::CriterionStatement::parse(format!(
                                "Verifier {criterion_id} covers its declared target."
                            ))
                            .expect("statement"),
                            command: CriterionCommand::parse(format!("verify {criterion_id}"))
                                .expect("command"),
                        },
                    })
                })
                .collect(),
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("accept criteria");
    repository
        .adopt_branch_basis(WorkBranchBasisChange {
            owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_work_revision: WorkRevision::new(2).expect("work r2"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            target_goal_revision: GoalRevision::INITIAL,
            target_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            source_ref: WorkChangeRef::parse(id("basis-source")).expect("source"),
        })
        .await
        .expect("adopt criteria");
}

fn decision(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    decision_id: &str,
    criterion_ids: &[String],
) -> NewWorkAcceptanceDecision {
    NewWorkAcceptanceDecision {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        decision_id: AcceptanceDecisionId::parse(decision_id).expect("decision"),
        work_revision: WorkRevision::new(2).expect("Work r2"),
        goal_revision: GoalRevision::INITIAL,
        branch_revision: WorkBranchRevision::new(3).expect("branch r3"),
        graph_revision: GraphRevision::INITIAL,
        criterion_set_revision: CriterionSetRevision::new(2).expect("criterion set r2"),
        subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
        subject_revision: hash('a'),
        accepted_gaps: criterion_ids
            .iter()
            .map(|criterion_id| AcceptedCriterionGap {
                criterion: criterion_ref(criterion_id),
                reason: AcceptanceGapReason::MissingEvidence,
                check_run_refs: Vec::new(),
            })
            .collect(),
        source_cursor: WorkChangeRef::parse(id("decision-cursor")).expect("cursor"),
    }
}

fn partial_check(
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
        check_run_id: CheckRunId::parse(check_run_id).expect("check"),
        graph_revision: GraphRevision::INITIAL,
        item: WorkItemRevisionRef {
            item_id: WorkItemId::root(),
            revision: WorkItemRevision::INITIAL,
        },
        attempt_id: WorkItemAttemptId::parse(attempt_id(branch_id)).expect("attempt"),
        criterion_set_revision: CriterionSetRevision::new(2).expect("criterion set r2"),
        criterion: criterion_ref(criterion_id),
        subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
        subject_revision: hash('a'),
        artifact_digest: None,
        run_ref: WorkChangeRef::parse(attempt_id(branch_id)).expect("run"),
        invocation_ref: WorkChangeRef::parse(id("invocation")).expect("invocation"),
        verifier_kind: CheckVerifierKind::Test,
        verifier_fingerprint: hash('b'),
        environment_fingerprint: hash('c'),
        outcome: CheckOutcome::Failed,
        error_kind: None,
        coverage: CheckCoverage::Partial,
        coverage_gaps: vec![CheckCoverageGap::TargetNotObserved],
        evidence_refs: vec![
            CheckEvidenceRef::parse("urn:astra:artifact:cloud:partial-check/result")
                .expect("evidence"),
        ],
        source_cursor: WorkChangeRef::parse(id("check-cursor")).expect("cursor"),
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

async fn decision_count(pool: &astra_core::SharedPool, owner_id: &str, work_id: &str) -> i64 {
    sqlx::query(
        "SELECT COUNT(*) AS count FROM work_acceptance_decisions
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("count decisions")
    .try_get("count")
    .expect("count")
}

async fn current_gap_count(pool: &astra_core::SharedPool, owner_id: &str, work_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_current_gap_acceptances
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("count current accepted gaps")
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn acceptance_is_canonical_idempotent_and_revision_bound() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_ids = [id("criterion-a"), id("criterion-b")];
    let decision_id = id("decision");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_criteria(&repository, &owner_id, &work_id, &branch_id, &criterion_ids).await;
    let mut no_subject = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("no-subject-decision"),
        &criterion_ids,
    );
    no_subject.branch_revision = WorkBranchRevision::new(2).expect("branch r2");
    assert!(matches!(
        repository.accept_gaps(no_subject).await,
        Err(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: astra_services::work::WorkAcceptanceBasisResource::Subject
        })
    ));
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;

    let first = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &decision_id,
        &criterion_ids,
    );
    let mut reordered = first.clone();
    reordered.accepted_gaps.reverse();
    let (left, right) = tokio::join!(
        repository.accept_gaps(first),
        repository.accept_gaps(reordered)
    );
    let left = left.expect("first acceptance");
    let right = right.expect("idempotent acceptance");
    assert_eq!(left.payload_hash, right.payload_hash);
    assert_eq!(left.decided_at, right.decided_at);
    assert_eq!(decision_count(&pool, &owner_id, &work_id).await, 1);
    assert_eq!(
        current_gap_count(&pool, &owner_id, &work_id).await,
        2,
        "one bounded current row is retained per accepted criterion"
    );
    let mut conflicting = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &decision_id,
        &criterion_ids,
    );
    conflicting.subject_revision = hash('f');
    assert!(matches!(
        repository.accept_gaps(conflicting).await,
        Err(WorkRepositoryError::Conflict {
            resource: astra_services::work::WorkConflictResource::AcceptanceDecisionIdentity
        })
    ));

    repository
        .revise_goal(WorkGoalChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::new(2).expect("Work r2"),
            expected_goal_revision: GoalRevision::INITIAL,
            goal: WorkGoal::parse("Changed Goal invalidates the old acceptance basis.")
                .expect("goal"),
            source_ref: WorkChangeRef::parse(id("goal-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("revise Goal");
    let stale = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("stale-decision"),
        &criterion_ids,
    );
    assert!(matches!(
        repository.accept_gaps(stale).await,
        Err(WorkRepositoryError::InvalidAcceptanceBasis { .. })
    ));
    assert_eq!(decision_count(&pool, &owner_id, &work_id).await, 1);
    assert_eq!(current_gap_count(&pool, &owner_id, &work_id).await, 2);
    let event_rows = sqlx::query(
        "SELECT event_seq, event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("Work events");
    assert_eq!(
        event_rows.len(),
        6,
        "rejected acceptance attempts must not produce semantic events"
    );
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
            "gaps_accepted",
            "goal_revised",
        ]
    );
    for (index, row) in event_rows.iter().enumerate() {
        assert_eq!(
            row.try_get::<i64, _>("event_seq").expect("sequence"),
            i64::try_from(index + 1).expect("sequence")
        );
    }

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn partial_evidence_must_reference_a_same_criterion_check_run() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_ids = [id("criterion-a"), id("criterion-b")];
    let check_run_id = id("check");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_criteria(&repository, &owner_id, &work_id, &branch_id, &criterion_ids).await;
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;
    repository
        .record_check_run(partial_check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_ids[0],
            &check_run_id,
        ))
        .await
        .expect("partial check");

    let mut accepted = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("accepted-decision"),
        &criterion_ids[..1],
    );
    accepted.accepted_gaps[0].reason = AcceptanceGapReason::PartialCoverage;
    accepted.accepted_gaps[0].check_run_refs =
        vec![CheckRunId::parse(&check_run_id).expect("check")];
    repository
        .accept_gaps(accepted)
        .await
        .expect("accept exact partial evidence");

    let mut falsely_stale = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("false-stale-decision"),
        &criterion_ids[..1],
    );
    falsely_stale.accepted_gaps[0].reason = AcceptanceGapReason::StaleEvidence;
    falsely_stale.accepted_gaps[0].check_run_refs =
        vec![CheckRunId::parse(&check_run_id).expect("check")];
    assert!(matches!(
        repository.accept_gaps(falsely_stale).await,
        Err(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: astra_services::work::WorkAcceptanceBasisResource::CheckRunApplicability
        })
    ));

    let mut mismatched = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("mismatched-decision"),
        &criterion_ids[1..],
    );
    mismatched.accepted_gaps[0].reason = AcceptanceGapReason::PartialCoverage;
    mismatched.accepted_gaps[0].check_run_refs =
        vec![CheckRunId::parse(&check_run_id).expect("check")];
    assert!(matches!(
        repository.accept_gaps(mismatched).await,
        Err(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: astra_services::work::WorkAcceptanceBasisResource::CheckRunCriterion
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
        .expect("advance current subject");
    let mut genuinely_stale = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &id("stale-evidence-decision"),
        &criterion_ids[..1],
    );
    genuinely_stale.branch_revision = WorkBranchRevision::new(4).expect("branch r4");
    genuinely_stale.subject_revision = hash('e');
    genuinely_stale.accepted_gaps[0].reason = AcceptanceGapReason::StaleEvidence;
    genuinely_stale.accepted_gaps[0].check_run_refs =
        vec![CheckRunId::parse(&check_run_id).expect("check")];
    repository
        .accept_gaps(genuinely_stale)
        .await
        .expect("stale evidence can be accepted only against the new current subject");

    assert_eq!(decision_count(&pool, &owner_id, &work_id).await, 2);
    assert_eq!(
        current_gap_count(&pool, &owner_id, &work_id).await,
        1,
        "a later exact decision replaces the same criterion projection instead of growing history"
    );
    let current_gap = sqlx::query(
        "SELECT decision_event_seq, subject_revision, gap_reason,
                resolved_check_refs_json
         FROM work_current_gap_acceptances
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND criterion_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&criterion_ids[0])
    .fetch_one(pool.get())
    .await
    .expect("current accepted gap");
    assert_eq!(
        current_gap
            .try_get::<i64, _>("decision_event_seq")
            .expect("event sequence"),
        8
    );
    assert_eq!(
        current_gap
            .try_get::<String, _>("subject_revision")
            .expect("subject revision"),
        hash('e').as_str()
    );
    assert_eq!(
        current_gap
            .try_get::<String, _>("gap_reason")
            .expect("gap reason"),
        "stale_evidence"
    );
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
            "check_recorded",
            "gaps_accepted",
            "subject_changed",
            "gaps_accepted",
        ],
        "invalid acceptance attempts must leave no event"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_gap_decisions_converge_by_work_event_order() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    let first_id = id("decision-a");
    let second_id = id("decision-b");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_criteria(
        &repository,
        &owner_id,
        &work_id,
        &branch_id,
        std::slice::from_ref(&criterion_id),
    )
    .await;
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;

    let first = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &first_id,
        std::slice::from_ref(&criterion_id),
    );
    let mut second = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &second_id,
        std::slice::from_ref(&criterion_id),
    );
    second.accepted_gaps[0].reason = AcceptanceGapReason::HumanJudgment;
    let (first_result, second_result) = tokio::join!(
        repository.accept_gaps(first),
        repository.accept_gaps(second)
    );
    first_result.expect("first concurrent decision");
    second_result.expect("second concurrent decision");

    let events = sqlx::query(
        "SELECT event_seq, source_ref FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'gaps_accepted'
         ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("acceptance events");
    assert_eq!(events.len(), 2);
    let winning_event_seq = events[1]
        .try_get::<i64, _>("event_seq")
        .expect("event sequence");
    let winning_decision_id = events[1]
        .try_get::<String, _>("source_ref")
        .expect("decision source");
    let projection = sqlx::query(
        "SELECT decision_id, decision_event_seq FROM work_current_gap_acceptances
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND criterion_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&criterion_id)
    .fetch_one(pool.get())
    .await
    .expect("current acceptance");
    assert_eq!(
        projection
            .try_get::<String, _>("decision_id")
            .expect("decision"),
        winning_decision_id
    );
    assert_eq!(
        projection
            .try_get::<i64, _>("decision_event_seq")
            .expect("event sequence"),
        winning_event_seq,
        "the Work event sequence, not completion time, orders concurrent decisions"
    );
    assert_eq!(current_gap_count(&pool, &owner_id, &work_id).await, 1);

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn event_retention_prunes_history_without_erasing_current_gap_acceptance() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let criterion_id = id("criterion");
    let check_run_id = id("accepted-check");
    let decision_id = id("decision");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    accept_criteria(
        &repository,
        &owner_id,
        &work_id,
        &branch_id,
        std::slice::from_ref(&criterion_id),
    )
    .await;
    establish_subject(&pool, &repository, &owner_id, &work_id, &branch_id).await;
    let recorded_check = repository
        .record_check_run(partial_check(
            &owner_id,
            &work_id,
            &branch_id,
            &criterion_id,
            &check_run_id,
        ))
        .await
        .expect("partial check");
    let mut accepted = decision(
        &owner_id,
        &work_id,
        &branch_id,
        &decision_id,
        std::slice::from_ref(&criterion_id),
    );
    accepted.accepted_gaps[0].reason = AcceptanceGapReason::PartialCoverage;
    accepted.accepted_gaps[0].check_run_refs =
        vec![CheckRunId::parse(&check_run_id).expect("check")];
    let recorded_acceptance = repository
        .accept_gaps(accepted)
        .await
        .expect("accept partial coverage");

    let projection = sqlx::query(
        "SELECT decision_id, decision_event_seq, resolved_check_refs_json,
                decision_payload_hash, subject_revision
         FROM work_current_gap_acceptances
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND criterion_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&criterion_id)
    .fetch_one(pool.get())
    .await
    .expect("current gap projection");
    assert_eq!(
        projection
            .try_get::<String, _>("decision_id")
            .expect("decision"),
        decision_id
    );
    assert_eq!(
        projection
            .try_get::<i64, _>("decision_event_seq")
            .expect("event sequence"),
        6
    );
    let resolved_refs: serde_json::Value = serde_json::from_str(
        &projection
            .try_get::<String, _>("resolved_check_refs_json")
            .expect("resolved refs"),
    )
    .expect("resolved refs JSON");
    assert_eq!(resolved_refs[0]["check_run_id"], check_run_id);
    assert_eq!(
        resolved_refs[0]["payload_hash"],
        recorded_check.payload_hash.as_str()
    );
    assert_ne!(
        projection
            .try_get::<String, _>("decision_payload_hash")
            .expect("resolved acceptance fact hash"),
        recorded_acceptance.payload_hash.as_str(),
        "the canonical current fact hash must include the resolved verifier payload hash"
    );

    // Jump to the fixed retention boundary instead of performing 9,998
    // irrelevant writes. Events 5 and 6 are the check and decision above.
    sqlx::query(
        "UPDATE work_event_sequences
         SET last_event_seq = 10004, retained_from_event_seq = 5
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("place sequence at evidence retention boundary");
    for suffix in ["new-check-a", "new-check-b"] {
        repository
            .record_check_run(partial_check(
                &owner_id,
                &work_id,
                &branch_id,
                &criterion_id,
                &id(suffix),
            ))
            .await
            .expect("append retained check");
    }

    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM work_check_runs
             WHERE owner_id = ? AND work_id = ? AND check_run_id = ?",
        )
        .bind(&owner_id)
        .bind(&work_id)
        .bind(&check_run_id)
        .fetch_one(pool.get())
        .await
        .expect("expired check count"),
        0,
        "check detail expires with its semantic event"
    );
    assert_eq!(
        decision_count(&pool, &owner_id, &work_id).await,
        0,
        "immutable decision history expires with its semantic event"
    );
    let retained_projection = sqlx::query(
        "SELECT decision_id, decision_event_seq, resolved_check_refs_json
         FROM work_current_gap_acceptances
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND criterion_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&criterion_id)
    .fetch_one(pool.get())
    .await
    .expect("retained current acceptance");
    assert_eq!(
        retained_projection
            .try_get::<String, _>("decision_id")
            .expect("decision"),
        decision_id
    );
    assert_eq!(
        retained_projection
            .try_get::<i64, _>("decision_event_seq")
            .expect("event sequence"),
        6,
        "history pruning must not rewrite acceptance causality"
    );
    assert_eq!(
        retained_projection
            .try_get::<String, _>("resolved_check_refs_json")
            .expect("resolved refs"),
        projection
            .try_get::<String, _>("resolved_check_refs_json")
            .expect("original refs"),
        "the exact verifier payload hash survives detail retention"
    );

    cleanup_owner(&pool, &owner_id).await;
}
