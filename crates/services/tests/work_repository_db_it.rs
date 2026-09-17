mod common;

use astra_services::work::{
    DatabaseWorkRepository, GoalRevision, WorkAttentionCursorAdvance, WorkAttentionCursorKind,
    WorkChangeReason, WorkChangeRef, WorkConflictResource, WorkEventCoverage, WorkEventPageLimit,
    WorkEventQuery, WorkEventSeq, WorkGenesis, WorkGoal, WorkGoalChange, WorkId, WorkOwnerId,
    WorkRepository, WorkRepositoryError, WorkRevision,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str, session_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        session_id,
        &id("intent"),
        "Repair the failing repository invariant.",
    )
}

fn goal_change(owner_id: &str, work_id: &str, goal: &str) -> WorkGoalChange {
    WorkGoalChange {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner id"),
        work_id: WorkId::parse(work_id).expect("work id"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        goal: WorkGoal::parse(goal).expect("goal"),
        source_ref: WorkChangeRef::parse(id("event")).expect("goal change ref"),
        reason: Some(WorkChangeReason::parse("User clarified the outcome.").expect("reason")),
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
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table} for test owner: {error}"));
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
        .expect("count column")
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn genesis_is_atomic_owner_scoped_and_materializes_real_roots() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    let created = repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("create Work genesis");
    assert_eq!(created.work.parts().work_revision.get(), 1);
    assert_eq!(created.work.parts().current_goal_revision.get(), 1);
    assert_eq!(created.work.parts().current_criteria_set_revision.get(), 1);
    assert_eq!(created.delivery_branch.parts().branch_revision.get(), 1);
    assert_eq!(
        created.delivery_branch.parts().current_graph_revision.get(),
        1
    );

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("load Work");
    assert_eq!(loaded, created);
    assert!(matches!(
        repository
            .load(
                &WorkOwnerId::parse(&other_owner_id).expect("other owner"),
                &WorkId::parse(&work_id).expect("work"),
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    let criterion_row = sqlx::query(
        "SELECT CAST(member_manifest_json AS CHAR) AS manifest_json,
                member_manifest_hash, accepted_by_kind, accepted_by_id
         FROM work_criterion_sets
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("initial criterion set");
    let criterion_manifest: serde_json::Value = serde_json::from_str(
        &criterion_row
            .try_get::<String, _>("manifest_json")
            .expect("criterion manifest"),
    )
    .expect("criterion manifest JSON");
    assert_eq!(criterion_manifest["schema_version"], 1);
    assert_eq!(criterion_manifest["members"], serde_json::json!([]));
    let criterion_hash: String = criterion_row.try_get("member_manifest_hash").expect("hash");
    assert_eq!(criterion_hash.len(), 71);
    assert_eq!(
        criterion_row
            .try_get::<String, _>("accepted_by_kind")
            .expect("criterion set actor kind"),
        "user"
    );
    assert_eq!(
        criterion_row
            .try_get::<String, _>("accepted_by_id")
            .expect("criterion set actor id"),
        owner_id
    );

    let graph_row = sqlx::query(
        "SELECT CAST(item_revision_manifest_json AS CHAR) AS items_json,
                CAST(edge_manifest_json AS CHAR) AS edges_json, manifest_hash, actor_kind, actor_id
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("initial graph");
    let item_manifest: serde_json::Value = serde_json::from_str(
        &graph_row
            .try_get::<String, _>("items_json")
            .expect("item manifest"),
    )
    .expect("item manifest JSON");
    assert_eq!(
        item_manifest,
        serde_json::json!([{"item_id": "root", "revision": 1}])
    );
    let edge_manifest: serde_json::Value = serde_json::from_str(
        &graph_row
            .try_get::<String, _>("edges_json")
            .expect("edge manifest"),
    )
    .expect("edge manifest JSON");
    assert_eq!(edge_manifest, serde_json::json!([]));
    assert_eq!(
        graph_row
            .try_get::<String, _>("manifest_hash")
            .expect("graph hash")
            .len(),
        71
    );
    assert_eq!(
        graph_row
            .try_get::<String, _>("actor_kind")
            .expect("graph actor kind"),
        "system"
    );
    assert_eq!(
        graph_row
            .try_get::<String, _>("actor_id")
            .expect("graph actor id"),
        "astra"
    );
    let root = sqlx::query(
        "SELECT r.item_kind, r.objective, r.expected_result, r.declaration_state, r.source_ref
         FROM work_items i
         JOIN work_item_revisions r
           ON r.owner_id = i.owner_id AND r.work_id = i.work_id
          AND r.item_id = i.item_id AND r.revision = 1
         WHERE i.owner_id = ? AND i.work_id = ? AND i.item_id = 'root'",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("canonical root item");
    assert_eq!(root.try_get::<String, _>("item_kind").unwrap(), "milestone");
    assert_eq!(
        root.try_get::<String, _>("objective").unwrap(),
        "Repair the failing repository invariant."
    );
    assert_eq!(
        root.try_get::<String, _>("declaration_state").unwrap(),
        "active"
    );
    assert!(
        !root
            .try_get::<String, _>("expected_result")
            .unwrap()
            .is_empty()
    );
    assert!(!root.try_get::<String, _>("source_ref").unwrap().is_empty());

    let session_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("internal session")
    .try_get("count")
    .expect("count");
    assert_eq!(session_count, 1);

    sqlx::query("DELETE FROM work_branches WHERE owner_id = ? AND work_id = ?")
        .bind(&owner_id)
        .bind(&work_id)
        .execute(pool.get())
        .await
        .expect("simulate a corrupt missing delivery branch");
    assert!(matches!(
        repository
            .load(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
            )
            .await,
        Err(WorkRepositoryError::Corrupt { .. })
    ));

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn session_binding_is_owner_scoped_and_same_owner_conflict_rolls_back_every_row() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let first_owner_id = id("owner");
    let second_owner_id = id("owner");
    let first_work_id = id("work");
    let same_owner_work_id = id("work");
    let cross_owner_work_id = id("work");
    let session_id = id("session");
    cleanup_owner(&pool, &first_owner_id).await;
    cleanup_owner(&pool, &second_owner_id).await;

    repository
        .create_genesis(genesis(
            &first_owner_id,
            &first_work_id,
            &id("branch"),
            &session_id,
        ))
        .await
        .expect("first Work");

    let same_owner_error = repository
        .create_genesis(genesis(
            &first_owner_id,
            &same_owner_work_id,
            &id("branch"),
            &session_id,
        ))
        .await
        .expect_err("one owner cannot reuse an internal session");
    assert!(matches!(
        same_owner_error,
        WorkRepositoryError::Conflict {
            resource: WorkConflictResource::InternalSessionIdentity
        }
    ));

    let cross_owner = repository
        .create_genesis(genesis(
            &second_owner_id,
            &cross_owner_work_id,
            &id("branch"),
            &session_id,
        ))
        .await
        .expect("opaque internal session identities are owner scoped");
    assert_eq!(cross_owner.work.parts().owner_id.as_str(), second_owner_id);

    for table in [
        "works",
        "work_goal_revisions",
        "work_criterion_sets",
        "work_items",
        "work_item_revisions",
        "work_graph_revisions",
        "work_graph_sequences",
        "work_branches",
        "work_proposal_sequences",
        "work_event_sequences",
        "work_attention_receipts",
        "work_events",
    ] {
        assert_eq!(
            count_work_rows(&pool, table, &first_owner_id, &same_owner_work_id).await,
            0,
            "failed same-owner genesis leaked {table}"
        );
    }

    let cross_owner_session_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&second_owner_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("cross-owner session count")
    .try_get("count")
    .expect("count");
    assert_eq!(
        cross_owner_session_count, 1,
        "a different owner has an independent internal session namespace"
    );

    cleanup_owner(&pool, &first_owner_id).await;
    cleanup_owner(&pool, &second_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn goal_revision_cas_preserves_branch_basis_and_rejects_stale_or_archived_writes() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &id("branch"), &id("session")))
        .await
        .expect("Work genesis");

    let change = goal_change(
        &owner_id,
        &work_id,
        "Repair the invariant and prove concurrent safety.",
    );
    let updated = repository
        .revise_goal(change.clone())
        .await
        .expect("revise Goal");
    assert_eq!(updated.work.parts().work_revision.get(), 2);
    assert_eq!(updated.work.parts().current_goal_revision.get(), 2);
    assert_eq!(
        updated.delivery_branch.parts().goal_revision_ref.get(),
        1,
        "a Goal change must make the old branch basis explicit, not rewrite history"
    );

    let goal_row = sqlx::query(
        "SELECT goal_text, source_kind, source_ref, accepted_by_kind, accepted_by_id, reason
         FROM work_goal_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("Goal revision 2");
    assert_eq!(
        goal_row.try_get::<String, _>("goal_text").expect("goal"),
        "Repair the invariant and prove concurrent safety."
    );
    assert_eq!(
        goal_row
            .try_get::<String, _>("source_kind")
            .expect("source kind"),
        "user_edit"
    );
    assert_eq!(
        goal_row
            .try_get::<String, _>("accepted_by_kind")
            .expect("actor kind"),
        "user"
    );
    assert_eq!(
        goal_row
            .try_get::<String, _>("accepted_by_id")
            .expect("actor id"),
        owner_id
    );
    assert_eq!(
        goal_row.try_get::<String, _>("reason").expect("reason"),
        "User clarified the outcome."
    );

    assert!(matches!(
        repository.revise_goal(change.clone()).await,
        Err(WorkRepositoryError::StaleGoalRevision {
            expected_work_revision,
            actual_work_revision,
            expected_goal_revision,
            actual_goal_revision,
        }) if expected_work_revision.get() == 1
            && actual_work_revision.get() == 2
            && expected_goal_revision.get() == 1
            && actual_goal_revision.get() == 2
    ));
    assert_eq!(
        count_work_rows(&pool, "work_goal_revisions", &owner_id, &work_id).await,
        2,
        "a stale CAS must not leave an orphan Goal revision"
    );

    let reused_source = WorkGoalChange {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        expected_work_revision: WorkRevision::new(2).expect("Work r2"),
        expected_goal_revision: GoalRevision::new(2).expect("Goal r2"),
        goal: WorkGoal::parse("A source identity cannot describe a different revision.")
            .expect("goal"),
        source_ref: change.source_ref.clone(),
        reason: None,
    };
    assert!(matches!(
        repository.revise_goal(reused_source).await,
        Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkEventIdentity
        })
    ));
    let unchanged = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("load after event identity conflict");
    assert_eq!(unchanged.work.parts().work_revision.get(), 2);
    assert_eq!(unchanged.work.parts().current_goal_revision.get(), 2);
    assert_eq!(
        count_work_rows(&pool, "work_goal_revisions", &owner_id, &work_id).await,
        2,
        "event failure must roll back the Goal revision"
    );
    let sequence = sqlx::query(
        "SELECT last_event_seq FROM work_event_sequences
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("event sequence after conflict")
    .try_get::<i64, _>("last_event_seq")
    .expect("event sequence");
    assert_eq!(sequence, 2, "failed events must not leave sequence gaps");

    let foreign_change = WorkGoalChange {
        owner_id: WorkOwnerId::parse(id("owner")).expect("foreign owner"),
        expected_work_revision: WorkRevision::new(2).expect("Work r2"),
        expected_goal_revision: GoalRevision::new(2).expect("Goal r2"),
        ..goal_change(&owner_id, &work_id, "A foreign owner must observe nothing.")
    };
    assert!(matches!(
        repository.revise_goal(foreign_change).await,
        Err(WorkRepositoryError::NotFound)
    ));

    sqlx::query("UPDATE works SET archived_at = NOW(6) WHERE owner_id = ? AND work_id = ?")
        .bind(&owner_id)
        .bind(&work_id)
        .execute(pool.get())
        .await
        .expect("archive Work fixture");
    let archived_change = WorkGoalChange {
        expected_work_revision: WorkRevision::new(2).expect("Work r2"),
        expected_goal_revision: GoalRevision::new(2).expect("Goal r2"),
        ..goal_change(&owner_id, &work_id, "This write must be rejected.")
    };
    assert!(matches!(
        repository.revise_goal(archived_change).await,
        Err(WorkRepositoryError::Archived)
    ));
    assert_eq!(
        count_work_rows(&pool, "work_goal_revisions", &owner_id, &work_id).await,
        2,
        "an archived Work must not gain a Goal revision"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_goal_revision_cas_has_one_winner_and_no_orphan_revision() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &id("branch"), &id("session")))
        .await
        .expect("Work genesis");

    let first = repository.clone();
    let second = repository.clone();
    let first_change = goal_change(&owner_id, &work_id, "First concurrent clarification.");
    let second_change = goal_change(&owner_id, &work_id, "Second concurrent clarification.");
    let (first_result, second_result) = tokio::join!(
        first.revise_goal(first_change),
        second.revise_goal(second_change)
    );
    let results = [first_result, second_result];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one compare-and-swap may win"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(WorkRepositoryError::StaleGoalRevision { .. })))
            .count(),
        1,
        "the losing writer must receive revision facts, not a text-classified error"
    );

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("load winner");
    assert_eq!(loaded.work.parts().work_revision.get(), 2);
    assert_eq!(loaded.work.parts().current_goal_revision.get(), 2);
    assert_eq!(
        count_work_rows(&pool, "work_goal_revisions", &owner_id, &work_id).await,
        2,
        "genesis plus exactly one winning revision must remain"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn event_retention_advances_in_constant_work_at_the_window_boundary() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &id("branch"), &id("session")))
        .await
        .expect("Work genesis");

    sqlx::query(
        "UPDATE work_event_sequences
         SET last_event_seq = 10000, retained_from_event_seq = 1
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("place event sequence at the retention boundary");

    repository
        .revise_goal(goal_change(
            &owner_id,
            &work_id,
            "Advance retention without scanning session or event history.",
        ))
        .await
        .expect("append boundary event");

    let sequence = sqlx::query(
        "SELECT last_event_seq, retained_from_event_seq
         FROM work_event_sequences WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("event sequence");
    assert_eq!(
        sequence.try_get::<i64, _>("last_event_seq").expect("head"),
        10001
    );
    assert_eq!(
        sequence
            .try_get::<i64, _>("retained_from_event_seq")
            .expect("retained floor"),
        2
    );
    let expired_event_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("expired event count")
    .try_get::<i64, _>("count")
    .expect("count");
    assert_eq!(
        expired_event_count, 0,
        "one append must prune exactly the event leaving the fixed window"
    );
    let boundary_event = sqlx::query(
        "SELECT event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq = 10001",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("new boundary event");
    assert_eq!(
        boundary_event
            .try_get::<String, _>("event_kind")
            .expect("event kind"),
        "goal_revised"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn attention_cursors_are_owner_scoped_monotonic_and_naturally_idempotent() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &id("branch"), &id("session")))
        .await
        .expect("Work genesis");

    let advance = |kind, through_event_seq| WorkAttentionCursorAdvance {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        kind,
        through_event_seq: WorkEventSeq::new(through_event_seq).expect("event sequence"),
    };
    let seen = repository
        .advance_attention_cursor(advance(WorkAttentionCursorKind::Seen, 1))
        .await
        .expect("mark genesis event seen");
    assert_eq!(seen.revision.get(), 2);
    assert_eq!(seen.seen_through_event_seq.map(WorkEventSeq::get), Some(1));
    assert_eq!(seen.delivered_through_event_seq, None);
    assert!(seen.seen_receipt_hash.is_some());
    assert_eq!(
        repository
            .advance_attention_cursor(advance(WorkAttentionCursorKind::Seen, 1))
            .await
            .expect("idempotent seen replay"),
        seen,
        "an exact replay must not synthesize a new receipt revision or timestamp"
    );

    let delivered = repository
        .advance_attention_cursor(advance(WorkAttentionCursorKind::Delivered, 1))
        .await
        .expect("mark genesis event delivered");
    assert_eq!(delivered.revision.get(), 3);
    assert_eq!(
        delivered.delivered_through_event_seq.map(WorkEventSeq::get),
        Some(1)
    );
    assert_eq!(
        delivered.seen_through_event_seq.map(WorkEventSeq::get),
        Some(1),
        "delivery and seen are independent durable facts"
    );

    assert!(matches!(
        repository
            .advance_attention_cursor(advance(WorkAttentionCursorKind::Seen, 2))
            .await,
        Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: 2,
            event_head: 1
        })
    ));
    let revision_after_rejection = sqlx::query(
        "SELECT receipt_revision FROM work_attention_receipts
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("receipt after future cursor rejection")
    .try_get::<i64, _>("receipt_revision")
    .expect("revision");
    assert_eq!(revision_after_rejection, 3);

    let event_writer = repository.clone();
    let cursor_writer = repository.clone();
    let goal = goal_change(
        &owner_id,
        &work_id,
        "Produce a second committed semantic event.",
    );
    let seen_through_old_head = advance(WorkAttentionCursorKind::Seen, 1);
    let (goal_result, old_cursor_result) = tokio::join!(
        event_writer.revise_goal(goal),
        cursor_writer.advance_attention_cursor(seen_through_old_head)
    );
    goal_result.expect("append Goal event");
    assert_eq!(
        old_cursor_result.expect("advance through the old head"),
        delivered,
        "a concurrently arriving event must remain unseen when it is beyond through_event_seq"
    );
    let first = repository.clone();
    let second = repository.clone();
    let first_advance = advance(WorkAttentionCursorKind::Seen, 2);
    let second_advance = first_advance.clone();
    let (left, right) = tokio::join!(
        first.advance_attention_cursor(first_advance),
        second.advance_attention_cursor(second_advance)
    );
    let left = left.expect("first concurrent cursor");
    let right = right.expect("second concurrent cursor");
    assert_eq!(left, right);
    assert_eq!(left.revision.get(), 4, "only one max advance may commit");
    assert_eq!(left.seen_through_event_seq.map(WorkEventSeq::get), Some(2));
    assert_eq!(
        repository
            .advance_attention_cursor(advance(WorkAttentionCursorKind::Seen, 1))
            .await
            .expect("older cursor is a no-op"),
        left
    );

    let foreign = WorkAttentionCursorAdvance {
        owner_id: WorkOwnerId::parse(&other_owner_id).expect("other owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        kind: WorkAttentionCursorKind::Seen,
        through_event_seq: WorkEventSeq::INITIAL,
    };
    assert!(matches!(
        repository.advance_attention_cursor(foreign).await,
        Err(WorkRepositoryError::NotFound)
    ));

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn event_pages_are_bounded_contiguous_owner_scoped_and_retention_explicit() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &id("branch"), &id("session")))
        .await
        .expect("Work genesis");
    repository
        .revise_goal(goal_change(
            &owner_id,
            &work_id,
            "Create a second semantic event for cursor pagination.",
        ))
        .await
        .expect("Goal revision");

    let event_query = |owner_id: &str, after_event_seq, limit| WorkEventQuery {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        after_event_seq,
        limit: WorkEventPageLimit::new(limit).expect("limit"),
    };
    let first = repository
        .list_events(event_query(&owner_id, None, 1))
        .await
        .expect("first event page");
    assert_eq!(first.coverage, WorkEventCoverage::Complete);
    assert_eq!(first.event_head.get(), 2);
    assert_eq!(first.retained_from_event_seq.get(), 1);
    assert_eq!(first.seen_through_event_seq, None);
    assert!(first.has_more);
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].event_seq.get(), 1);
    assert_eq!(
        first.events[0].kind,
        astra_services::work::WorkEventKind::WorkCreated
    );

    let second = repository
        .list_events(event_query(&owner_id, first.next_after_event_seq, 1))
        .await
        .expect("second event page");
    assert!(!second.has_more);
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].event_seq.get(), 2);
    assert_eq!(
        second.events[0].kind,
        astra_services::work::WorkEventKind::GoalRevised
    );
    assert_eq!(second.next_after_event_seq.map(WorkEventSeq::get), Some(2));

    assert!(matches!(
        repository
            .list_events(event_query(
                &owner_id,
                Some(WorkEventSeq::new(3).expect("event 3")),
                10,
            ))
            .await,
        Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: 3,
            event_head: 2
        })
    ));
    assert!(matches!(
        repository
            .list_events(event_query(&other_owner_id, None, 10))
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    repository
        .advance_attention_cursor(WorkAttentionCursorAdvance {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            kind: WorkAttentionCursorKind::Seen,
            through_event_seq: WorkEventSeq::INITIAL,
        })
        .await
        .expect("read event 1");
    sqlx::query(
        "DELETE FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("simulate completed retention of event 1");
    sqlx::query(
        "UPDATE work_event_sequences SET retained_from_event_seq = 2
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("advance retained floor");
    let expired = repository
        .list_events(event_query(&owner_id, None, 10))
        .await
        .expect("expired coverage page");
    assert_eq!(expired.coverage, WorkEventCoverage::Expired);
    assert_eq!(expired.retained_from_event_seq.get(), 2);
    assert_eq!(
        expired.seen_through_event_seq.map(WorkEventSeq::get),
        Some(1)
    );
    assert_eq!(expired.events.len(), 1);
    assert_eq!(expired.events[0].event_seq.get(), 2);

    sqlx::query(
        "DELETE FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("simulate a retained event gap");
    assert!(matches!(
        repository
            .list_events(event_query(&owner_id, None, 10))
            .await,
        Err(WorkRepositoryError::Corrupt {
            entity: "Work event page",
            ..
        })
    ));

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}
