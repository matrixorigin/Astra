mod common;

use astra_services::work::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionKind, CriterionRevision,
    CriterionRevisionRef, CriterionSetMemberChange, CriterionSetRevision, CriterionStatement,
    DatabaseWorkRepository, NewWorkCriterion, WorkChangeReason, WorkChangeRef, WorkCriteriaChange,
    WorkGenesis, WorkId, WorkOwnerId, WorkRepository, WorkRepositoryError, WorkRevision,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(owner_id: &str, work_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        &id("branch"),
        &id("session"),
        &id("intent"),
        "Implement and prove the acceptance contract.",
    )
}

fn new_criterion(id: &str, kind: CriterionKind, statement: &str) -> CriterionSetMemberChange {
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
    CriterionSetMemberChange::New(NewWorkCriterion {
        criterion_id: CriterionId::parse(id).expect("criterion id"),
        definition,
    })
}

fn criteria_change(
    owner_id: &str,
    work_id: &str,
    members: Vec<CriterionSetMemberChange>,
) -> WorkCriteriaChange {
    WorkCriteriaChange {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        members,
        source_ref: WorkChangeRef::parse(id("event")).expect("source"),
        reason: Some(WorkChangeReason::parse("User accepted Done when.").expect("reason")),
    }
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("work_runtime_event_outbox", "owner_id"),
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_proposals", "owner_id"),
        ("work_proposal_sequences", "owner_id"),
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
    let owner_id = id("owner");
    let work_id = id("work");
    let criterion_a = id("criterion-a");
    let criterion_b = id("criterion-b");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let accepted = repository
        .accept_criteria(criteria_change(
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
        .expect("accept criteria");
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

    let cleared = repository
        .accept_criteria(WorkCriteriaChange {
            expected_work_revision: WorkRevision::new(2).expect("Work r2"),
            expected_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            members: Vec::new(),
            ..criteria_change(&owner_id, &work_id, Vec::new())
        })
        .await
        .expect("accept explicit empty set");
    assert_eq!(cleared.work.parts().work_revision.get(), 3);
    assert_eq!(cleared.work.parts().current_criteria_set_revision.get(), 3);
    let empty_manifest: String = sqlx::query(
        "SELECT CAST(member_manifest_json AS CHAR) AS manifest_json
         FROM work_criterion_sets WHERE owner_id = ? AND work_id = ? AND revision = 3",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("empty set r3")
    .try_get("manifest_json")
    .expect("manifest");
    let empty: serde_json::Value = serde_json::from_str(&empty_manifest).expect("empty manifest");
    assert_eq!(empty["members"], serde_json::json!([]));

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn invalid_or_missing_criterion_members_roll_back_the_work_cas() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let missing_id = CriterionId::parse(id("missing")).expect("missing id");
    let missing_ref = CriterionRevisionRef {
        criterion_id: missing_id.clone(),
        revision: CriterionRevision::INITIAL,
    };
    assert!(matches!(
        repository
            .accept_criteria(criteria_change(
                &owner_id,
                &work_id,
                vec![CriterionSetMemberChange::Existing(missing_ref)],
            ))
            .await,
        Err(WorkRepositoryError::MissingCriterionRevisions { missing })
            if missing.len() == 1 && missing[0].criterion_id == missing_id
    ));

    let duplicate_id = id("duplicate");
    assert!(matches!(
        repository
            .accept_criteria(criteria_change(
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

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_criterion_set_cas_has_one_complete_winner() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id))
        .await
        .expect("genesis");

    let first = repository.clone();
    let second = repository.clone();
    let first_change = criteria_change(
        &owner_id,
        &work_id,
        vec![new_criterion(
            &id("criterion"),
            CriterionKind::CommandCheck,
            "The command succeeds.",
        )],
    );
    let second_change = criteria_change(
        &owner_id,
        &work_id,
        vec![new_criterion(
            &id("criterion"),
            CriterionKind::TestCheck,
            "The second targeted test passes.",
        )],
    );
    let (first_result, second_result) = tokio::join!(
        first.accept_criteria(first_change),
        second.accept_criteria(second_change)
    );
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(WorkRepositoryError::StaleCriteriaRevision { .. })
            ))
            .count(),
        1
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

    cleanup_owner(&pool, &owner_id).await;
}
