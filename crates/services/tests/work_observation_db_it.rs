mod common;

use astra_services::work::{
    AcceptanceDecisionId, AcceptanceGapReason, AcceptedCriterionGap, CheckCoverage,
    CheckEvidenceRef, CheckOutcome, CheckRunId, CheckVerifierKind, CriterionCommand,
    CriterionDefinition, CriterionId, CriterionRevision, CriterionRevisionRef,
    CriterionSetMemberChange, CriterionSetRevision, CriterionStatement, DatabaseWorkRepository,
    GoalRevision, GraphRevision, NewWorkAcceptanceDecision, NewWorkCheckRun, NewWorkCriterion,
    NewWorkItem, ObservationScope, RevisionAlignment, WorkBranchBasisChange, WorkBranchId,
    WorkBranchRevision, WorkBranchSubjectChange, WorkChangeReason, WorkChangeRef, WorkContentHash,
    WorkCriteriaChange, WorkDeliveryStatus, WorkGenesis, WorkGoal, WorkGoalChange, WorkGraphChange,
    WorkGraphItemChange, WorkId, WorkItemAttemptId, WorkItemEdge, WorkItemEdgeKind, WorkItemId,
    WorkItemKind, WorkItemRevision, WorkItemRevisionRef, WorkItemText, WorkObservationQuery,
    WorkObservationSatisfactionEvidenceRef, WorkOwnerId, WorkRepository, WorkRepositoryError,
    WorkRetentionState, WorkRevision, WorkSubjectRef,
};
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
        "Ship a causally coherent Work observation.",
    )
}

fn observation(owner_id: &str, work_id: &str) -> WorkObservationQuery {
    WorkObservationQuery {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
    }
}

fn new_item(item_id: &str, kind: WorkItemKind) -> WorkGraphItemChange {
    WorkGraphItemChange::New(NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item"),
        kind,
        objective: WorkItemText::parse(format!("Complete {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} is verified"))
            .expect("expected result"),
    })
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

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn declared_work_observation_is_bounded_content_addressed_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let first_item = id("milestone");
    let second_item = id("task");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let criterion_id = id("criterion");
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
                definition: CriterionDefinition::HumanReview {
                    statement: CriterionStatement::parse(
                        "A reviewer accepts the declared Work projection.",
                    )
                    .expect("statement"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-event")).expect("source"),
            reason: Some(WorkChangeReason::parse("Accepted the review boundary.").expect("reason")),
        })
        .await
        .expect("accept criteria");
    repository
        .revise_goal(WorkGoalChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::new(2).expect("Work r2"),
            expected_goal_revision: GoalRevision::INITIAL,
            goal: WorkGoal::parse("Expose a bounded and verifiable declared-Work snapshot.")
                .expect("goal"),
            source_ref: WorkChangeRef::parse(id("goal-event")).expect("source"),
            reason: Some(
                WorkChangeReason::parse("Clarified the observable outcome.").expect("reason"),
            ),
        })
        .await
        .expect("revise goal");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![
                new_item(&second_item, WorkItemKind::Task),
                new_item(&first_item, WorkItemKind::Milestone),
            ],
            edges: vec![WorkItemEdge {
                predecessor_item_id: WorkItemId::parse(&first_item).expect("predecessor"),
                successor_item_id: WorkItemId::parse(&second_item).expect("successor"),
                kind: WorkItemEdgeKind::Dependency,
            }],
            source_ref: WorkChangeRef::parse(id("graph-event")).expect("source"),
            reason: Some(
                WorkChangeReason::parse("Declared the first ready frontier.").expect("reason"),
            ),
        })
        .await
        .expect("replace graph");

    let first = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe Work");
    let repeated = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("repeat observation");
    assert_eq!(
        first, repeated,
        "wall clock must not perturb report identity"
    );
    assert_eq!(first.scope(), ObservationScope::DeclaredWork);
    assert_eq!(first.source_revisions().len(), 6);
    assert!(first.coverage_gaps().is_empty());
    assert_eq!(first.overview().work_revision.get(), 3);
    assert_eq!(first.overview().goal.revision.get(), 2);
    assert_eq!(first.overview().criteria.revision.get(), 2);
    assert_eq!(first.overview().criteria.member_count, 1);
    assert_eq!(first.overview().graph.revision.get(), 2);
    assert_eq!(first.overview().graph.item_count, 2);
    assert_eq!(first.overview().graph.edge_count, 1);
    assert_eq!(
        first.overview().delivery_branch.goal_alignment,
        RevisionAlignment::Behind
    );
    assert_eq!(
        first.overview().delivery_branch.criteria_alignment,
        RevisionAlignment::Behind
    );
    assert_eq!(
        first.overview().delivery.status,
        WorkDeliveryStatus::BranchBasisOutOfDate
    );
    assert_eq!(first.overview().delivery.satisfied_criterion_count, 0);
    assert_eq!(first.overview().retention_state, WorkRetentionState::Active);
    let wire = serde_json::to_vec(&first).expect("serialize report");
    assert!(wire.len() < 64 * 1024);
    let value: serde_json::Value = serde_json::from_slice(&wire).expect("report JSON");
    assert!(
        value["overview"]["delivery_branch"]
            .get("session_id")
            .is_none()
    );

    assert!(matches!(
        repository
            .observe_declared_work(observation(&other_owner_id, &work_id))
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn delivery_is_ready_only_from_current_exact_evidence_or_acceptance() {
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

    let empty = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe empty criteria");
    assert_eq!(
        empty.overview().delivery.status,
        WorkDeliveryStatus::CriteriaNotAccepted,
        "an empty hard-criterion set must never become vacuously ready"
    );

    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: CriterionStatement::parse(
                        "The exact delivery subject passes the registered verifier.",
                    )
                    .expect("statement"),
                    command: CriterionCommand::parse("verify exact delivery subject")
                        .expect("command"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("accept criterion");
    let behind = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe branch behind criteria");
    assert_eq!(
        behind.overview().delivery.status,
        WorkDeliveryStatus::BranchBasisOutOfDate
    );

    repository
        .adopt_branch_basis(WorkBranchBasisChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_work_revision: WorkRevision::new(2).expect("work r2"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            target_goal_revision: GoalRevision::INITIAL,
            target_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            source_ref: WorkChangeRef::parse(id("basis-source")).expect("source"),
        })
        .await
        .expect("adopt current criteria");
    let no_subject = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe missing subject");
    assert_eq!(
        no_subject.overview().delivery.status,
        WorkDeliveryStatus::SubjectUnavailable
    );

    let subject_ref = WorkSubjectRef::parse("workspace/repository/head").expect("subject");
    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(2).expect("branch r2"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: hash('a'),
            source_ref: WorkChangeRef::parse(id("subject-source")).expect("source"),
        })
        .await
        .expect("materialize subject");
    let unverified = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe unverified subject");
    assert_eq!(
        unverified.overview().delivery.status,
        WorkDeliveryStatus::VerificationRequired
    );
    assert_eq!(unverified.overview().delivery.remaining_criterion_count, 1);

    let run_id = id("run");
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
    .expect("persist exact WorkItem run");
    let criterion = CriterionRevisionRef {
        criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
        revision: CriterionRevision::INITIAL,
    };
    let check_produced_at = chrono::Utc::now();
    let check_expires_at = check_produced_at + chrono::Duration::minutes(10);
    let check_id = id("check");
    repository
        .record_check_run(NewWorkCheckRun {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            check_run_id: CheckRunId::parse(&check_id).expect("check"),
            graph_revision: GraphRevision::INITIAL,
            item: WorkItemRevisionRef {
                item_id: WorkItemId::root(),
                revision: WorkItemRevision::INITIAL,
            },
            attempt_id: WorkItemAttemptId::parse(&run_id).expect("attempt"),
            criterion_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            criterion: criterion.clone(),
            subject_ref: subject_ref.clone(),
            subject_revision: hash('a'),
            artifact_digest: Some(hash('b')),
            run_ref: WorkChangeRef::parse(&run_id).expect("run"),
            invocation_ref: WorkChangeRef::parse(id("invocation")).expect("invocation"),
            verifier_kind: CheckVerifierKind::Test,
            verifier_fingerprint: hash('c'),
            environment_fingerprint: hash('d'),
            outcome: CheckOutcome::Passed,
            error_kind: None,
            coverage: CheckCoverage::Complete,
            coverage_gaps: Vec::new(),
            evidence_refs: vec![
                CheckEvidenceRef::parse("urn:astra:artifact:cloud:delivery/check")
                    .expect("evidence"),
            ],
            source_cursor: WorkChangeRef::parse(id("check-source")).expect("source"),
            produced_at: check_produced_at,
            expires_at: Some(check_expires_at),
        })
        .await
        .expect("record exact passing check");
    let checked = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe checked delivery");
    assert_eq!(
        checked.overview().delivery.status,
        WorkDeliveryStatus::ReadyForReview
    );
    assert_eq!(checked.overview().delivery.fresh_check_count, 1);
    assert_eq!(checked.overview().delivery.accepted_gap_count, 0);
    assert!(matches!(
        checked.satisfaction_evidence_refs(),
        [WorkObservationSatisfactionEvidenceRef::CheckRun {
            check_run_id,
            ..
        }] if check_run_id.as_str() == check_id
    ));
    assert_eq!(
        checked
            .overview()
            .delivery
            .freshness_valid_until
            .expect("expiring evidence boundary")
            .timestamp_micros(),
        check_expires_at.timestamp_micros()
    );
    assert!(
        checked.overview().event_head > unverified.overview().event_head,
        "evidence changes must advance the report's causal watermark"
    );

    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(3).expect("branch r3"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: hash('e'),
            source_ref: WorkChangeRef::parse(id("subject-advanced")).expect("source"),
        })
        .await
        .expect("advance exact subject");
    let stale = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe stale evidence");
    assert_eq!(
        stale.overview().delivery.status,
        WorkDeliveryStatus::VerificationRequired
    );
    assert_eq!(stale.overview().delivery.fresh_check_count, 0);
    assert!(
        stale.satisfaction_evidence_refs().is_empty(),
        "a prior subject's evidence identity must not leak into the current causal cut"
    );

    let decision_identity = id("decision");
    repository
        .accept_gaps(NewWorkAcceptanceDecision {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            decision_id: AcceptanceDecisionId::parse(&decision_identity).expect("decision"),
            work_revision: WorkRevision::new(2).expect("work r2"),
            goal_revision: GoalRevision::INITIAL,
            branch_revision: WorkBranchRevision::new(4).expect("branch r4"),
            graph_revision: GraphRevision::INITIAL,
            criterion_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            subject_ref,
            subject_revision: hash('e'),
            accepted_gaps: vec![AcceptedCriterionGap {
                criterion,
                reason: AcceptanceGapReason::MissingEvidence,
                check_run_refs: Vec::new(),
            }],
            source_cursor: WorkChangeRef::parse(id("acceptance-source")).expect("source"),
        })
        .await
        .expect("accept exact current gap");
    let accepted = repository
        .observe_declared_work(observation(&owner_id, &work_id))
        .await
        .expect("observe accepted delivery");
    assert_eq!(
        accepted.overview().delivery.status,
        WorkDeliveryStatus::ReadyForReview
    );
    assert_eq!(accepted.overview().delivery.fresh_check_count, 0);
    assert_eq!(accepted.overview().delivery.accepted_gap_count, 1);
    assert!(matches!(
        accepted.satisfaction_evidence_refs(),
        [WorkObservationSatisfactionEvidenceRef::AcceptanceDecision { decision_id, .. }]
            if decision_id.as_str() == decision_identity
    ));

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn missing_current_source_fails_closed_instead_of_fabricating_partial_facts() {
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
    sqlx::query(
        "DELETE FROM work_goal_revisions WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("remove current Goal source");

    assert!(matches!(
        repository
            .observe_declared_work(observation(&owner_id, &work_id))
            .await,
        Err(WorkRepositoryError::Corrupt { .. })
    ));

    cleanup_owner(&pool, &owner_id).await;
}
