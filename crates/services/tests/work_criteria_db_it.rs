mod common;

use astra_services::work::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionKind, CriterionRevision,
    CriterionSetRevision, CriterionStatement, DatabaseWorkRepository, WorkGenesis, WorkId,
    WorkOwnerId, WorkRepository, WorkRepositoryError, WorkRevision,
};
use sqlx::Row;

fn genesis(owner_id: &str, work_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        &format!("{work_id}-branch"),
        &common::id("session"),
        &common::id("intent"),
        "Implement and prove the acceptance contract.",
    )
}

fn new_criterion(
    id: &str,
    kind: CriterionKind,
    statement: &str,
) -> astra_services::work::WorkCriteriaProposalMember {
    let statement = CriterionStatement::parse(statement).expect("statement");
    let definition = match kind {
        CriterionKind::CommandCheck => CriterionDefinition::CommandCheck {
            statement,
            command: CriterionCommand::parse("make check").expect("command"),
        },
        CriterionKind::TestCheck => CriterionDefinition::TestCheck {
            statement,
            command: CriterionCommand::parse("cargo test -p astra-services").expect("test command"),
        },
        CriterionKind::HumanReview => CriterionDefinition::HumanReview { statement },
        unsupported => panic!("unsupported fixture criterion kind: {unsupported:?}"),
    };
    astra_services::work::WorkCriteriaProposalMember::New {
        criterion_id: CriterionId::parse(id).expect("criterion id"),
        definition,
    }
}

fn proposal(
    owner_id: &str,
    work_id: &str,
    members: Vec<astra_services::work::WorkCriteriaProposalMember>,
) -> astra_services::work::NewWorkCriteriaProposal {
    use astra_services::work::*;
    NewWorkCriteriaProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(format!("{work_id}-branch")).expect("branch"),
        proposal_id: WorkProposalId::parse(common::id("proposal")).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        members,
        source_kind: WorkProposalSourceKind::Model,
        source_ref: WorkChangeRef::parse(common::id("source")).expect("source"),
    }
}

async fn count_work_rows(
    pool: &astra_core::SharedPool,
    table: &str,
    owner_id: &str,
    work_id: &str,
) -> i64 {
    let statement =
        format!("SELECT COUNT(*) AS count FROM {table} WHERE owner_id = ? AND work_id = ?");
    sqlx::query(&statement)
        .bind(owner_id)
        .bind(work_id)
        .fetch_one(pool.get())
        .await
        .unwrap_or_else(|error| panic!("count {table}: {error}"))
        .try_get("count")
        .expect("count")
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn accepted_criteria_are_immutable_canonical_and_leave_branch_basis_explicit() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let criterion_a = common::id("criterion-a");
    let criterion_b = common::id("criterion-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let recorded = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            vec![
                new_criterion(
                    &criterion_b,
                    CriterionKind::HumanReview,
                    "A reviewer accepts the interaction quality.",
                ),
                new_criterion(
                    &criterion_a,
                    CriterionKind::TestCheck,
                    "The targeted repository tests pass.",
                ),
            ],
        ))
        .await
        .expect("record criteria proposal");
    repository
        .accept_criteria_proposal(common::criteria_acceptance(
            &recorded,
            &common::id("accept"),
        ))
        .await
        .expect("accept criteria");
    let accepted = repository
        .load(&recorded.proposal.owner_id, &recorded.proposal.work_id)
        .await
        .expect("load accepted Work");
    assert_eq!(accepted.work.parts().work_revision.get(), 2);
    assert_eq!(accepted.work.parts().current_criteria_set_revision.get(), 2);
    assert_eq!(
        accepted
            .delivery_branch
            .parts()
            .criteria_set_revision_ref
            .get(),
        1,
        "accepted Done when must not rewrite a branch's historical basis"
    );

    let set_row = sqlx::query(
        "SELECT parent_revision, CAST(member_manifest_json AS CHAR) AS manifest_json,
                member_manifest_hash, member_count, accepted_by_kind, accepted_by_id
         FROM work_criterion_sets
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("criterion set r2");
    assert_eq!(
        set_row
            .try_get::<i64, _>("parent_revision")
            .expect("parent"),
        1
    );
    assert_eq!(
        set_row
            .try_get::<String, _>("accepted_by_kind")
            .expect("actor kind"),
        "user"
    );
    assert_eq!(
        set_row
            .try_get::<String, _>("accepted_by_id")
            .expect("actor id"),
        owner_id
    );
    assert_eq!(
        set_row
            .try_get::<String, _>("member_manifest_hash")
            .expect("hash")
            .len(),
        71
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &set_row
            .try_get::<String, _>("manifest_json")
            .expect("manifest"),
    )
    .expect("manifest JSON");
    let members = manifest["members"].as_array().expect("member array");
    assert_eq!(members.len(), 2);
    assert_eq!(
        set_row
            .try_get::<i32, _>("member_count")
            .expect("member count"),
        members.len() as i32,
        "summary count must be derived from the immutable manifest in the same transaction"
    );
    assert_eq!(members[0]["criterion_id"], criterion_a);
    assert_eq!(members[0]["revision"], 1);
    assert_eq!(members[1]["criterion_id"], criterion_b);

    let revisions = sqlx::query(
        "SELECT criterion_id, criterion_kind, CAST(definition_json AS CHAR) AS definition_json
         FROM work_criterion_revisions
         WHERE owner_id = ? AND work_id = ? ORDER BY criterion_id",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("criterion revisions");
    assert_eq!(revisions.len(), 2);
    assert_eq!(
        revisions[0]
            .try_get::<String, _>("criterion_kind")
            .expect("kind"),
        "test_check"
    );
    assert_eq!(
        revisions[1]
            .try_get::<String, _>("criterion_kind")
            .expect("kind"),
        "human_review"
    );
    for row in revisions {
        let definition: serde_json::Value = serde_json::from_str(
            &row.try_get::<String, _>("definition_json")
                .expect("definition"),
        )
        .expect("definition JSON");
        assert_eq!(definition["schema_version"], 1);
        assert!(definition["definition"]["statement"].as_str().is_some());
        if definition["definition"]["kind"] != "human_review" {
            assert!(definition["definition"]["command"].as_str().is_some());
        }
    }

    let mut empty = proposal(&owner_id, &work_id, Vec::new());
    empty.expected_work_revision = WorkRevision::new(2).expect("Work r2");
    empty.expected_criteria_set_revision = CriterionSetRevision::new(2).expect("set r2");
    assert!(matches!(
        repository.propose_criteria(empty).await,
        Err(WorkRepositoryError::InvalidMutation { .. })
    ));
    let unchanged = repository
        .load(&recorded.proposal.owner_id, &recorded.proposal.work_id)
        .await
        .expect("load after rejected empty proposal");
    assert_eq!(unchanged, accepted);
    assert_eq!(
        count_work_rows(&pool, "work_criterion_sets", &owner_id, &work_id).await,
        2
    );
    assert_eq!(
        count_work_rows(&pool, "work_criterion_revisions", &owner_id, &work_id).await,
        2
    );
    assert_eq!(
        count_work_rows(&pool, "work_proposals", &owner_id, &work_id).await,
        1
    );

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn invalid_or_missing_criterion_proposals_leave_work_unchanged() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let missing_id = CriterionId::parse(common::id("missing")).expect("missing id");
    assert!(matches!(
        repository
            .propose_criteria(proposal(
                &owner_id,
                &work_id,
                vec![astra_services::work::WorkCriteriaProposalMember::Existing { criterion_id: missing_id.clone(), revision: CriterionRevision::INITIAL }],
            ))
            .await,
        Err(WorkRepositoryError::MissingCriterionRevisions { missing })
            if missing.len() == 1 && missing[0].criterion_id == missing_id
    ));

    let duplicate_id = common::id("duplicate");
    assert!(matches!(
        repository
            .propose_criteria(proposal(
                &owner_id,
                &work_id,
                vec![
                    new_criterion(&duplicate_id, CriterionKind::TestCheck, "The test passes."),
                    new_criterion(
                        &duplicate_id,
                        CriterionKind::HumanReview,
                        "A reviewer approves."
                    ),
                ],
            ))
            .await,
        Err(WorkRepositoryError::InvalidMutation { .. })
    ));

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("load unchanged Work");
    assert_eq!(loaded.work.parts().work_revision.get(), 1);
    assert_eq!(loaded.work.parts().current_criteria_set_revision.get(), 1);
    assert_eq!(
        count_work_rows(&pool, "work_criterion_sets", &owner_id, &work_id).await,
        1
    );
    assert_eq!(
        count_work_rows(&pool, "work_criteria", &owner_id, &work_id).await,
        0
    );

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_criterion_set_cas_has_one_complete_winner() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let first_proposal = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            vec![new_criterion(
                &common::id("criterion"),
                CriterionKind::CommandCheck,
                "The command succeeds.",
            )],
        ))
        .await
        .expect("first proposal");
    let second_proposal = repository
        .propose_criteria(proposal(
            &owner_id,
            &work_id,
            vec![new_criterion(
                &common::id("criterion"),
                CriterionKind::TestCheck,
                "The second targeted test passes.",
            )],
        ))
        .await
        .expect("second proposal");
    let (first_result, second_result) = tokio::join!(
        repository.accept_criteria_proposal(common::criteria_acceptance(
            &first_proposal,
            &common::id("first-resolution")
        )),
        repository.accept_criteria_proposal(common::criteria_acceptance(
            &second_proposal,
            &common::id("second-resolution")
        ))
    );
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(WorkRepositoryError::InvalidWorkProposalBasis {
                    resource: astra_services::work::WorkProposalBasisResource::WorkRevision
                })
            ))
            .count(),
        1
    );
    let mut statuses = Vec::new();
    for recorded in [&first_proposal, &second_proposal] {
        let stored = repository
            .load_criteria_proposal(
                &recorded.proposal.owner_id,
                &recorded.proposal.work_id,
                &recorded.proposal.proposal_id,
            )
            .await
            .expect("load raced proposal")
            .expect("proposal remains discoverable");
        assert_eq!(
            stored.resolution.is_some(),
            stored.status == astra_services::work::WorkProposalStatus::Accepted
        );
        statuses.push(stored.status);
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == astra_services::work::WorkProposalStatus::Accepted)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == astra_services::work::WorkProposalStatus::Pending)
            .count(),
        1,
        "the losing proposal remains pending without a resolution"
    );
    assert_eq!(
        count_work_rows(&pool, "work_criterion_sets", &owner_id, &work_id).await,
        2,
        "genesis plus one accepted set"
    );
    assert_eq!(
        count_work_rows(&pool, "work_criteria", &owner_id, &work_id).await,
        1,
        "losing criteria identities must roll back"
    );
    assert_eq!(
        count_work_rows(&pool, "work_criterion_revisions", &owner_id, &work_id).await,
        1,
        "losing criterion revisions must roll back"
    );

    let loaded = repository
        .load(
            &first_proposal.proposal.owner_id,
            &first_proposal.proposal.work_id,
        )
        .await
        .expect("load winning Work");
    assert_eq!(loaded.work.parts().work_revision.get(), 2);
    assert_eq!(loaded.work.parts().current_criteria_set_revision.get(), 2);
    let accepted_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_events WHERE owner_id = ? AND work_id = ? AND event_kind = 'criteria_accepted'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("accepted event count");
    assert_eq!(accepted_events, 1);

    common::cleanup_work_owner(&pool, &owner_id).await;
}
