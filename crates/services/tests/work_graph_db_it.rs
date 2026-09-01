mod common;

use astra_services::work::{
    DatabaseWorkRepository, GraphRevision, NewWorkItem, WorkBranchId, WorkBranchRevision,
    WorkChangeReason, WorkChangeRef, WorkGenesis, WorkGraphChange, WorkGraphItemChange, WorkId,
    WorkItemEdge, WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemRevision,
    WorkItemRevisionRef, WorkItemText, WorkOwnerId, WorkRepository, WorkRepositoryError,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &id("session"),
        &id("intent"),
        "Deliver a proven dependency-aware change.",
    )
}

fn new_item(item_id: &str, kind: WorkItemKind) -> WorkGraphItemChange {
    WorkGraphItemChange::New(NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item id"),
        kind,
        objective: WorkItemText::parse(format!("Complete {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} has objective evidence"))
            .expect("expected result"),
    })
}

fn graph_change(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    items: Vec<WorkGraphItemChange>,
    edges: Vec<WorkItemEdge>,
) -> WorkGraphChange {
    WorkGraphChange {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        items,
        edges,
        source_ref: WorkChangeRef::parse(id("event")).expect("source"),
        reason: Some(WorkChangeReason::parse("Refined the task graph.").expect("reason")),
    }
}

fn dependency(predecessor: &str, successor: &str) -> WorkItemEdge {
    WorkItemEdge {
        predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
        successor_item_id: WorkItemId::parse(successor).expect("successor"),
        kind: WorkItemEdgeKind::Dependency,
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

async fn scalar_count(
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
async fn graph_replacement_is_canonical_immutable_and_branch_local() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let first = id("item-a");
    let second = id("item-b");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let branch = repository
        .replace_graph(graph_change(
            &owner_id,
            &work_id,
            &branch_id,
            vec![
                new_item(&second, WorkItemKind::Task),
                new_item(&first, WorkItemKind::Milestone),
            ],
            vec![dependency(&first, &second)],
        ))
        .await
        .expect("replace graph");
    assert_eq!(branch.parts().branch_revision.get(), 2);
    assert_eq!(branch.parts().basis_graph_revision.get(), 1);
    assert_eq!(branch.parts().current_graph_revision.get(), 2);

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("load Work");
    assert_eq!(
        loaded.work.parts().work_revision.get(),
        1,
        "branch graph churn must not create false Goal/criteria CAS conflicts"
    );
    assert_eq!(
        loaded.delivery_branch.parts().current_graph_revision.get(),
        2
    );

    let graph_row = sqlx::query(
        "SELECT parent_revision, CAST(item_revision_manifest_json AS CHAR) AS items_json,
                CAST(edge_manifest_json AS CHAR) AS edges_json, manifest_hash, patch_hash,
                item_count, edge_count
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("graph r2");
    assert_eq!(
        graph_row
            .try_get::<i64, _>("parent_revision")
            .expect("parent"),
        1
    );
    let manifest_hash = graph_row
        .try_get::<String, _>("manifest_hash")
        .expect("manifest hash");
    let patch_hash = graph_row
        .try_get::<String, _>("patch_hash")
        .expect("patch hash");
    assert_eq!(manifest_hash.len(), 71);
    assert_eq!(patch_hash.len(), 71);
    assert_ne!(
        manifest_hash, patch_hash,
        "the admitted replacement hash must bind item definitions, not impersonate the graph-root hash"
    );
    let items: serde_json::Value =
        serde_json::from_str(&graph_row.try_get::<String, _>("items_json").expect("items"))
            .expect("items JSON");
    assert_eq!(items[0]["item_id"], first);
    assert_eq!(items[0]["revision"], 1);
    assert_eq!(items[1]["item_id"], second);
    let edges: serde_json::Value =
        serde_json::from_str(&graph_row.try_get::<String, _>("edges_json").expect("edges"))
            .expect("edges JSON");
    assert_eq!(edges[0]["predecessor_item_id"], first);
    assert_eq!(edges[0]["successor_item_id"], second);
    assert_eq!(
        graph_row
            .try_get::<i32, _>("item_count")
            .expect("item count"),
        items.as_array().expect("item array").len() as i32
    );
    assert_eq!(
        graph_row
            .try_get::<i32, _>("edge_count")
            .expect("edge count"),
        edges.as_array().expect("edge array").len() as i32
    );

    let item_rows = sqlx::query(
        "SELECT item_id, revision, item_kind, declaration_state
         FROM work_item_revisions WHERE owner_id = ? AND work_id = ? ORDER BY item_id",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("item revisions");
    assert_eq!(item_rows.len(), 3);
    assert_eq!(
        item_rows[0].try_get::<String, _>("item_id").expect("id"),
        first
    );
    assert_eq!(
        item_rows[0]
            .try_get::<String, _>("item_kind")
            .expect("kind"),
        "milestone"
    );
    assert_eq!(
        item_rows[1].try_get::<String, _>("item_id").expect("id"),
        second
    );
    assert_eq!(
        item_rows[1]
            .try_get::<String, _>("item_kind")
            .expect("kind"),
        "task"
    );
    assert_eq!(
        item_rows[2].try_get::<String, _>("item_id").expect("id"),
        "root",
        "graph replacement must not erase the durable genesis root"
    );
    for row in item_rows {
        assert_eq!(row.try_get::<i64, _>("revision").expect("revision"), 1);
        assert_eq!(
            row.try_get::<String, _>("declaration_state")
                .expect("state"),
            "active"
        );
    }

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn missing_item_reference_rolls_back_branch_and_revision_allocation() {
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
    let initial_item_count = scalar_count(&pool, "work_items", &owner_id, &work_id).await;
    let initial_item_revision_count =
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await;
    let missing = WorkItemRevisionRef {
        item_id: WorkItemId::parse(id("missing")).expect("missing item"),
        revision: WorkItemRevision::INITIAL,
    };

    let error = repository
        .replace_graph(graph_change(
            &owner_id,
            &work_id,
            &branch_id,
            vec![WorkGraphItemChange::Existing(missing.clone())],
            Vec::new(),
        ))
        .await
        .expect_err("missing immutable item revision");
    assert!(matches!(
        error,
        WorkRepositoryError::MissingWorkItemRevisions { missing: actual }
            if actual == vec![missing]
    ));

    let branch_row = sqlx::query(
        "SELECT branch_revision, current_graph_revision FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("branch");
    assert_eq!(
        branch_row
            .try_get::<i64, _>("branch_revision")
            .expect("branch revision"),
        1
    );
    assert_eq!(
        branch_row
            .try_get::<i64, _>("current_graph_revision")
            .expect("graph"),
        1
    );
    let sequence: i64 = sqlx::query(
        "SELECT last_revision FROM work_graph_sequences WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("sequence")
    .try_get("last_revision")
    .expect("last revision");
    assert_eq!(sequence, 1);
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_id, &work_id).await,
        1
    );
    assert_eq!(
        scalar_count(&pool, "work_items", &owner_id, &work_id).await,
        initial_item_count,
        "a rejected graph must not leave item identity residue"
    );
    assert_eq!(
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await,
        initial_item_revision_count,
        "a rejected graph must not leave item revision residue"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_same_branch_graph_changes_have_one_cas_winner_without_residue() {
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
    let initial_item_revision_count =
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await;
    let first_id = id("winner-a");
    let second_id = id("winner-b");
    let first_repository = repository.clone();
    let second_repository = repository.clone();
    let first = first_repository.replace_graph(graph_change(
        &owner_id,
        &work_id,
        &branch_id,
        vec![new_item(&first_id, WorkItemKind::Task)],
        Vec::new(),
    ));
    let second = second_repository.replace_graph(graph_change(
        &owner_id,
        &work_id,
        &branch_id,
        vec![new_item(&second_id, WorkItemKind::Task)],
        Vec::new(),
    ));
    let (first_result, second_result) = tokio::join!(first, second);
    let first_won = first_result.is_ok();
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(WorkRepositoryError::StaleGraphRevision { .. })))
            .count(),
        1
    );
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_id, &work_id).await,
        2
    );
    assert_eq!(
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await,
        initial_item_revision_count + 1,
        "only the winning graph may materialize an item revision"
    );
    let winning_item_id = if first_won { &first_id } else { &second_id };
    let losing_item_id = if first_won { &second_id } else { &first_id };
    let persisted_item_ids = sqlx::query(
        "SELECT item_id FROM work_item_revisions
         WHERE owner_id = ? AND work_id = ? ORDER BY item_id",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("persisted item revisions")
    .into_iter()
    .map(|row| row.try_get::<String, _>("item_id").expect("item id"))
    .collect::<Vec<_>>();
    assert!(persisted_item_ids.iter().any(|id| id == "root"));
    assert!(persisted_item_ids.iter().any(|id| id == winning_item_id));
    assert!(!persisted_item_ids.iter().any(|id| id == losing_item_id));
    let sequence: i64 = sqlx::query(
        "SELECT last_revision FROM work_graph_sequences WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("sequence")
    .try_get("last_revision")
    .expect("last revision");
    assert_eq!(
        sequence, 2,
        "losing CAS must roll back its allocated revision"
    );

    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn different_users_allocate_graph_revisions_independently() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_a = id("owner-a");
    let owner_b = id("owner-b");
    let work_a = id("work-a");
    let work_b = id("work-b");
    let branch_a = id("branch-a");
    let branch_b = id("branch-b");
    cleanup_owner(&pool, &owner_a).await;
    cleanup_owner(&pool, &owner_b).await;
    let (genesis_a, genesis_b) = tokio::join!(
        repository.create_genesis(genesis(&owner_a, &work_a, &branch_a)),
        repository.create_genesis(genesis(&owner_b, &work_b, &branch_b)),
    );
    genesis_a.expect("owner A genesis");
    genesis_b.expect("owner B genesis");

    let repository_a = repository.clone();
    let repository_b = repository.clone();
    let (result_a, result_b) = tokio::join!(
        repository_a.replace_graph(graph_change(
            &owner_a,
            &work_a,
            &branch_a,
            vec![new_item(&id("item-a"), WorkItemKind::Task)],
            Vec::new(),
        )),
        repository_b.replace_graph(graph_change(
            &owner_b,
            &work_b,
            &branch_b,
            vec![new_item(&id("item-b"), WorkItemKind::Task)],
            Vec::new(),
        )),
    );
    assert_eq!(
        result_a
            .expect("owner A graph")
            .parts()
            .current_graph_revision
            .get(),
        2
    );
    assert_eq!(
        result_b
            .expect("owner B graph")
            .parts()
            .current_graph_revision
            .get(),
        2
    );
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_a, &work_a).await,
        2
    );
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_b, &work_b).await,
        2
    );

    cleanup_owner(&pool, &owner_a).await;
    cleanup_owner(&pool, &owner_b).await;
}
