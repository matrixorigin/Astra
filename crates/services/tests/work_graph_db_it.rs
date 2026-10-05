mod common;

use astra_services::work::{
    DatabaseWorkRepository, NewWorkItem, WorkGenesis, WorkId, WorkItemEdge, WorkItemEdgeKind,
    WorkItemId, WorkItemKind, WorkItemRevision, WorkItemRevisionRef, WorkItemText, WorkOwnerId,
    WorkRepository, WorkRepositoryError,
};
use sqlx::Row;

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &common::id("session"),
        &common::id("intent"),
        "Deliver a proven dependency-aware change.",
    )
}

fn new_item(item_id: &str, kind: WorkItemKind) -> NewWorkItem {
    NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item id"),
        kind,
        objective: WorkItemText::parse(format!("Complete {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} has objective evidence"))
            .expect("expected result"),
    }
}

fn proposal(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    additions: Vec<NewWorkItem>,
    dependencies: Vec<WorkItemEdge>,
) -> astra_services::work::NewWorkPlanProposal {
    use astra_services::work::*;
    NewWorkPlanProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        proposal_id: WorkProposalId::parse(common::id("proposal")).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        additions,
        revisions: Vec::new(),
        dependencies,
        dependency_removals: Vec::new(),
        source_ref: WorkChangeRef::parse(common::id("event")).expect("source"),
        source_kind: WorkProposalSourceKind::Model,
        reason: WorkChangeReason::parse("Refined the task graph.").expect("reason"),
    }
}

fn dependency(predecessor: &str, successor: &str) -> WorkItemEdge {
    WorkItemEdge {
        predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
        successor_item_id: WorkItemId::parse(successor).expect("successor"),
        kind: WorkItemEdgeKind::Dependency,
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
async fn accepted_graph_is_canonical_immutable_and_branch_local() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let first = common::id("item-a");
    let second = common::id("item-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let recorded = repository
        .propose_plan(proposal(
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
        .expect("record graph proposal");
    let accepted = repository
        .accept_plan_proposal(common::plan_acceptance(&recorded, &common::id("accept")))
        .await
        .expect("accept graph proposal");
    let resolution = accepted.resolution.expect("accepted resolution");
    assert_eq!(resolution.result_branch_revision.expect("branch").get(), 2);
    assert_eq!(resolution.result_graph_revision.expect("graph").get(), 2);

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
    assert_eq!(loaded.delivery_branch.parts().branch_revision.get(), 2);
    assert_eq!(loaded.delivery_branch.parts().basis_graph_revision.get(), 1);

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
        "the admitted proposal hash must bind item definitions, not impersonate the graph-root hash"
    );
    let items: serde_json::Value =
        serde_json::from_str(&graph_row.try_get::<String, _>("items_json").expect("items"))
            .expect("items JSON");
    assert_eq!(items[0]["item_id"], first);
    assert_eq!(items[0]["revision"], 1);
    assert_eq!(items[1]["item_id"], second);
    assert_eq!(items[2]["item_id"], "root");
    assert_eq!(items.as_array().expect("item array").len(), 3);
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

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn missing_item_reference_rolls_back_branch_and_revision_allocation() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let initial_item_count = scalar_count(&pool, "work_items", &owner_id, &work_id).await;
    let initial_item_revision_count =
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await;
    let missing = WorkItemRevisionRef {
        item_id: WorkItemId::parse(common::id("missing")).expect("missing item"),
        revision: WorkItemRevision::INITIAL,
    };

    let mut invalid = proposal(&owner_id, &work_id, &branch_id, Vec::new(), Vec::new());
    invalid
        .revisions
        .push(astra_services::work::WorkItemRevisionChange::new(
            missing.item_id.clone(),
            missing.revision,
            WorkItemKind::Task,
            WorkItemText::parse("Revise a missing item").expect("objective"),
            WorkItemText::parse("This proposal must not materialize").expect("result"),
            astra_services::work::WorkItemDeclarationState::Active,
        ));
    assert!(matches!(
        repository.propose_plan(invalid).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::WorkItemRevision
        })
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

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_same_branch_graph_changes_have_one_cas_winner_without_residue() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let initial_item_revision_count =
        scalar_count(&pool, "work_item_revisions", &owner_id, &work_id).await;
    let first_id = common::id("winner-a");
    let second_id = common::id("winner-b");
    let first = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            vec![new_item(&first_id, WorkItemKind::Task)],
            Vec::new(),
        ))
        .await
        .expect("first proposal");
    let second = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            vec![new_item(&second_id, WorkItemKind::Task)],
            Vec::new(),
        ))
        .await
        .expect("second proposal");
    let (first_result, second_result) = tokio::join!(
        repository
            .accept_plan_proposal(common::plan_acceptance(&first, &common::id("accept-first"))),
        repository.accept_plan_proposal(common::plan_acceptance(
            &second,
            &common::id("accept-second")
        )),
    );
    let first_won = first_result.is_ok();
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(WorkRepositoryError::InvalidWorkProposalBasis {
                    resource: astra_services::work::WorkProposalBasisResource::BranchRevision
                })
            ))
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

    for (proposal, won) in [(&first, first_won), (&second, !first_won)] {
        let stored = repository
            .load_plan_proposal(
                &proposal.proposal.owner_id,
                &proposal.proposal.work_id,
                &proposal.proposal.proposal_id,
            )
            .await
            .expect("load raced proposal")
            .expect("recorded proposal");
        assert_eq!(
            stored.status,
            if won {
                astra_services::work::WorkProposalStatus::Accepted
            } else {
                astra_services::work::WorkProposalStatus::Pending
            }
        );
        assert_eq!(stored.resolution.is_some(), won);
    }
    let accepted_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_events WHERE owner_id = ? AND work_id = ? AND event_kind = 'graph_replaced'",
    ).bind(&owner_id).bind(&work_id).fetch_one(pool.get()).await.expect("accepted events");
    assert_eq!(accepted_events, 1);

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn different_users_allocate_graph_revisions_independently() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_a = common::id("owner-a");
    let owner_b = common::id("owner-b");
    let work_a = common::id("work-a");
    let work_b = common::id("work-b");
    let branch_a = common::id("branch-a");
    let branch_b = common::id("branch-b");
    let (genesis_a, genesis_b) = tokio::join!(
        repository.create_genesis(genesis(&owner_a, &work_a, &branch_a)),
        repository.create_genesis(genesis(&owner_b, &work_b, &branch_b)),
    );
    genesis_a.expect("owner A genesis");
    genesis_b.expect("owner B genesis");

    let proposed_a = repository
        .propose_plan(proposal(
            &owner_a,
            &work_a,
            &branch_a,
            vec![new_item(&common::id("item-a"), WorkItemKind::Task)],
            Vec::new(),
        ))
        .await
        .expect("owner A proposal");
    let proposed_b = repository
        .propose_plan(proposal(
            &owner_b,
            &work_b,
            &branch_b,
            vec![new_item(&common::id("item-b"), WorkItemKind::Task)],
            Vec::new(),
        ))
        .await
        .expect("owner B proposal");
    let (result_a, result_b) = tokio::join!(
        repository.accept_plan_proposal(common::plan_acceptance(
            &proposed_a,
            &common::id("accept-a")
        )),
        repository.accept_plan_proposal(common::plan_acceptance(
            &proposed_b,
            &common::id("accept-b")
        )),
    );
    for result in [result_a, result_b] {
        let resolution = result
            .expect("independent owner acceptance")
            .resolution
            .expect("resolution");
        assert_eq!(
            resolution
                .result_graph_revision
                .expect("graph revision")
                .get(),
            2
        );
        assert_eq!(
            resolution
                .result_branch_revision
                .expect("branch revision")
                .get(),
            2
        );
    }
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_a, &work_a).await,
        2
    );
    assert_eq!(
        scalar_count(&pool, "work_graph_revisions", &owner_b, &work_b).await,
        2
    );

    common::cleanup_work_owner(&pool, &owner_a).await;
    common::cleanup_work_owner(&pool, &owner_b).await;
}
