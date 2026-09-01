mod common;

use astra_services::work::{
    DatabaseWorkRepository, GraphRevision, InternalSessionId, NewWorkItem, WorkBranchId,
    WorkBranchRevision, WorkChangeRef, WorkGraphChange, WorkGraphItemChange, WorkId, WorkItemEdge,
    WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemRevision, WorkItemRevisionRef,
    WorkItemText, WorkOwnerId, WorkRepository, WorkRepositoryError, WorkTaskGraphQuery,
};
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    session_id: &str,
) -> astra_services::work::WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        session_id,
        &id("intent"),
        "Maintain one coherent plan snapshot for the root loop.",
    )
}

fn item(item_id: &str) -> NewWorkItem {
    NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item"),
        kind: WorkItemKind::Task,
        objective: WorkItemText::parse(format!("Implement {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} is verified"))
            .expect("expected result"),
    }
}

fn dependency(from: &str, to: &str) -> WorkItemEdge {
    WorkItemEdge {
        predecessor_item_id: WorkItemId::parse(from).expect("from"),
        successor_item_id: WorkItemId::parse(to).expect("to"),
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
async fn public_branch_identity_resolves_one_active_owner_scoped_runtime_binding() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");

    let binding = repository
        .load_branch_runtime_binding(&owner, &work, &branch)
        .await
        .expect("runtime binding");
    assert_eq!(binding.work_id, work);
    assert_eq!(binding.branch_id, branch);
    assert_eq!(binding.session_id.as_str(), session_id);

    for (other_owner, other_work, other_branch) in [
        (id("other-owner"), work_id.clone(), branch_id.clone()),
        (owner_id.clone(), id("other-work"), branch_id.clone()),
        (owner_id.clone(), work_id.clone(), id("other-branch")),
    ] {
        assert!(matches!(
            repository
                .load_branch_runtime_binding(
                    &WorkOwnerId::parse(other_owner).expect("owner"),
                    &WorkId::parse(other_work).expect("work"),
                    &WorkBranchId::parse(other_branch).expect("branch"),
                )
                .await,
            Err(WorkRepositoryError::NotFound)
        ));
    }

    sqlx::query(
        "UPDATE work_branches SET archived_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .execute(pool.get())
    .await
    .expect("archive branch");
    assert!(matches!(
        repository
            .load_branch_runtime_binding(&owner, &work, &branch)
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    sqlx::query(
        "UPDATE work_branches SET archived_at = NULL
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .execute(pool.get())
    .await
    .expect("restore branch");
    sqlx::query("UPDATE works SET archived_at = NOW(6) WHERE owner_id = ? AND work_id = ?")
        .bind(&owner_id)
        .bind(&work_id)
        .execute(pool.get())
        .await
        .expect("archive Work");
    assert!(matches!(
        repository
            .load_branch_runtime_binding(&owner, &work, &branch)
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn session_item_runtime_binding_accepts_only_the_active_item_in_the_bound_graph() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let task_id = id("task");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![WorkGraphItemChange::New(item(&task_id))],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("graph");
    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let session = InternalSessionId::parse(&session_id).expect("session");
    let active_item = WorkItemRevisionRef {
        item_id: WorkItemId::parse(&task_id).expect("item"),
        revision: WorkItemRevision::INITIAL,
    };

    let binding = repository
        .load_session_item_runtime_binding(&owner, &session, &work, &branch, &active_item)
        .await
        .expect("active graph item binding");
    assert_eq!(binding.work_id, work);
    assert_eq!(binding.branch_id, branch);
    assert_eq!(
        binding.graph_revision,
        GraphRevision::new(2).expect("revision")
    );

    let missing_revision = WorkItemRevisionRef {
        item_id: active_item.item_id.clone(),
        revision: WorkItemRevision::new(2).expect("revision"),
    };
    assert!(matches!(
        repository
            .load_session_item_runtime_binding(&owner, &session, &work, &branch, &missing_revision)
            .await,
        Err(WorkRepositoryError::NotFound)
    ));

    // A graph reference is not sufficient authority on its own: an item that
    // was retired after the graph snapshot was written is never delegable.
    sqlx::query(
        "UPDATE work_item_revisions SET declaration_state = 'cancelled'
         WHERE owner_id = ? AND work_id = ? AND item_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&task_id)
    .execute(pool.get())
    .await
    .expect("retire item");
    assert!(matches!(
        repository
            .load_session_item_runtime_binding(&owner, &session, &work, &branch, &active_item)
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn session_plan_context_is_bounded_canonical_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let task_a = id("task-a");
    let task_b = id("task-b");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![
                WorkGraphItemChange::New(item(&task_b)),
                WorkGraphItemChange::New(item(&task_a)),
            ],
            edges: vec![dependency(&task_a, &task_b)],
            source_ref: WorkChangeRef::parse(id("graph-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("graph");

    let context = repository
        .load_plan_context_for_session(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&session_id).expect("session"),
        )
        .await
        .expect("plan context");
    assert_eq!(context.items().len(), 2);
    assert_eq!(context.dependencies(), [dependency(&task_a, &task_b)]);
    assert_eq!(context.items()[0].item_id.as_str(), task_a);
    assert_eq!(context.items()[1].item_id.as_str(), task_b);
    assert_eq!(context.basis().work_id.as_str(), work_id);
    assert_eq!(context.basis().branch_id.as_str(), branch_id);
    assert_eq!(
        context.basis().graph_revision,
        GraphRevision::new(2).expect("revision")
    );
    let encoded = serde_json::to_string(&context).expect("wire");
    assert!(
        encoded.len() < 64 * 1024,
        "context was {} bytes",
        encoded.len()
    );
    assert!(!encoded.contains(&owner_id));
    assert!(!encoded.contains(&session_id));

    let fork_branch_id = id("fork-branch");
    let fork_session_id = id("fork-session");
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, event_count, metadata,
          project_id, created_at, updated_at, last_active_at)
         VALUES (?, ?, NULL, NULL, 'active', 0, NULL, NULL, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&fork_session_id)
    .bind(&owner_id)
    .execute(pool.get())
    .await
    .expect("fork session");
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id,
          origin_branch_id, fork_cursor, goal_revision_ref, criteria_set_revision_ref,
          basis_graph_revision, current_graph_revision)
         VALUES (?, ?, ?, 1, ?, ?, ?, 1, 1, 2, 2)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&fork_branch_id)
    .bind(&fork_session_id)
    .bind(&branch_id)
    .bind(id("fork-cursor"))
    .execute(pool.get())
    .await
    .expect("fork branch");
    sqlx::query(
        "INSERT INTO work_proposal_sequences
         (owner_id, work_id, branch_id, last_proposal_seq) VALUES (?, ?, ?, 0)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&fork_branch_id)
    .execute(pool.get())
    .await
    .expect("fork proposal sequence");
    let fork_context = repository
        .load_plan_context_for_session(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&fork_session_id).expect("fork session"),
        )
        .await
        .expect("fork plan context");
    assert_eq!(fork_context.basis().branch_id.as_str(), fork_branch_id);
    assert_eq!(fork_context.basis().branch_revision.get(), 1);
    assert_eq!(fork_context.basis().branch_basis_graph_revision.get(), 2);
    assert_eq!(fork_context.basis().graph_revision.get(), 2);
    assert_eq!(fork_context.items(), context.items());

    let foreign_owner = WorkOwnerId::parse(id("foreign-owner")).expect("foreign owner");
    let missing_session = InternalSessionId::parse(id("missing-session")).expect("session");
    assert!(matches!(
        repository
            .load_plan_context_for_session(
                &foreign_owner,
                &InternalSessionId::parse(&session_id).expect("session")
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    assert!(matches!(
        repository
            .load_plan_context_for_session(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &missing_session
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn plan_context_rejects_corrupt_hash_and_missing_item_revision() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let task_id = id("task");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![WorkGraphItemChange::New(item(&task_id))],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("graph");
    let original_hash: String = sqlx::query_scalar(
        "SELECT manifest_hash FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("manifest hash");
    sqlx::query(
        "UPDATE work_graph_revisions SET manifest_hash = ?
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(format!("sha256:{}", "f".repeat(64)))
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("corrupt hash");
    let lightweight_binding = repository
        .load_session_plan_binding(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&session_id).expect("session"),
        )
        .await
        .expect("binding admission does not materialize the plan payload");
    assert_eq!(lightweight_binding.work_id.as_str(), work_id);
    assert_eq!(lightweight_binding.branch_id.as_str(), branch_id);
    assert!(matches!(
        repository
            .load_session_plan_binding(
                &WorkOwnerId::parse(id("foreign-owner")).expect("foreign owner"),
                &InternalSessionId::parse(&session_id).expect("session")
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    assert!(matches!(
        repository
            .load_plan_context_for_session(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &InternalSessionId::parse(&session_id).expect("session")
            )
            .await,
        Err(WorkRepositoryError::Corrupt {
            entity: "Work graph manifest",
            ..
        })
    ));
    sqlx::query(
        "UPDATE work_graph_revisions SET manifest_hash = ?
         WHERE owner_id = ? AND work_id = ? AND revision = 2",
    )
    .bind(original_hash)
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("restore hash");
    sqlx::query(
        "DELETE FROM work_item_revisions
         WHERE owner_id = ? AND work_id = ? AND item_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&task_id)
    .execute(pool.get())
    .await
    .expect("remove item revision");
    assert!(matches!(
        repository
            .load_plan_context_for_session(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &InternalSessionId::parse(&session_id).expect("session")
            )
            .await,
        Err(WorkRepositoryError::MissingWorkItemRevisions { missing }) if missing.len() == 1
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn plan_context_racing_graph_advance_is_never_torn() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let task_id = id("task");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");

    const READERS: usize = 16;
    let barrier = Arc::new(Barrier::new(READERS + 1));
    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..READERS {
        let repository = repository.clone();
        let owner_id = owner_id.clone();
        let session_id = session_id.clone();
        let barrier = barrier.clone();
        readers.spawn(async move {
            barrier.wait().await;
            repository
                .load_plan_context_for_session(
                    &WorkOwnerId::parse(owner_id).expect("owner"),
                    &InternalSessionId::parse(session_id).expect("session"),
                )
                .await
        });
    }
    barrier.wait().await;
    let advanced = repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![WorkGraphItemChange::New(item(&task_id))],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("advance graph");
    assert_eq!(
        advanced.parts().current_graph_revision,
        GraphRevision::new(2).expect("revision")
    );
    while let Some(result) = readers.join_next().await {
        let context = result.expect("reader task").expect("coherent context");
        let basis = context.basis();
        match basis.graph_revision.get() {
            1 => {
                assert_eq!(basis.branch_revision.get(), 1);
                assert_eq!(basis.graph_item_count, 1);
                assert_eq!(context.items().len(), 1);
                assert_eq!(context.items()[0].item_id.as_str(), "root");
            }
            2 => {
                assert_eq!(basis.branch_revision.get(), 2);
                assert_eq!(basis.graph_item_count, 1);
                assert_eq!(context.items().len(), 1);
                assert_eq!(context.items()[0].item_id.as_str(), task_id);
            }
            revision => panic!("unexpected graph revision {revision}"),
        }
        assert!(context.dependencies().is_empty());
    }
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn maximum_active_frontier_uses_one_bounded_item_fetch() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let items = (0..256)
        .map(|index| WorkGraphItemChange::New(item(&format!("task-{index:03}"))))
        .collect();
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items,
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("maximum-frontier")).expect("source"),
            reason: None,
        })
        .await
        .expect("maximum graph");
    let context = repository
        .load_plan_context_for_session(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&session_id).expect("session"),
        )
        .await
        .expect("maximum plan context");
    assert_eq!(context.basis().graph_item_count, 256);
    assert_eq!(context.items().len(), 256);
    assert_eq!(context.items()[0].item_id.as_str(), "task-000");
    assert_eq!(context.items()[255].item_id.as_str(), "task-255");
    assert!(context.dependencies().is_empty());
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn public_task_graph_pages_are_owner_scoped_and_fail_closed_across_replan() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let task_a = id("task-a");
    let task_b = id("task-b");
    let task_c = id("task-c");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let graph = repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![
                WorkGraphItemChange::New(item(&task_b)),
                WorkGraphItemChange::New(item(&task_a)),
            ],
            edges: vec![dependency(&task_a, &task_b)],
            source_ref: WorkChangeRef::parse(id("graph-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("graph");
    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let first = repository
        .load_task_graph_page(
            WorkTaskGraphQuery::new(
                owner.clone(),
                work.clone(),
                branch.clone(),
                None,
                0,
                1,
                0,
                1,
            )
            .expect("query"),
        )
        .await
        .expect("first page");
    assert_eq!(first.items().entries.len(), 1);
    assert_eq!(first.items().entries[0].item_id.as_str(), task_a);
    assert_eq!(first.dependencies().entries, [dependency(&task_a, &task_b)]);
    let next = first.next_cursor().cloned().expect("next cursor");
    assert_eq!(next.graph_revision, graph.parts().current_graph_revision);
    let terminal = repository
        .load_task_graph_page(
            WorkTaskGraphQuery::new(
                owner.clone(),
                work.clone(),
                branch.clone(),
                Some(next.graph_revision),
                next.item_offset,
                1,
                next.dependency_offset,
                1,
            )
            .expect("continuation"),
        )
        .await
        .expect("terminal page");
    assert_eq!(terminal.items().entries[0].item_id.as_str(), task_b);
    assert!(terminal.next_cursor().is_none());

    let advanced = repository
        .replace_graph(WorkGraphChange {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            expected_branch_revision: graph.parts().branch_revision,
            expected_graph_revision: graph.parts().current_graph_revision,
            items: vec![
                WorkGraphItemChange::Existing(WorkItemRevisionRef {
                    item_id: WorkItemId::parse(&task_a).expect("task a"),
                    revision: WorkItemRevision::INITIAL,
                }),
                WorkGraphItemChange::Existing(WorkItemRevisionRef {
                    item_id: WorkItemId::parse(&task_b).expect("task b"),
                    revision: WorkItemRevision::INITIAL,
                }),
                WorkGraphItemChange::New(item(&task_c)),
            ],
            edges: vec![dependency(&task_a, &task_b), dependency(&task_b, &task_c)],
            source_ref: WorkChangeRef::parse(id("replan")).expect("source"),
            reason: None,
        })
        .await
        .expect("replan");
    assert!(matches!(
        repository
            .load_task_graph_page(
                WorkTaskGraphQuery::new(
                    owner.clone(),
                    work.clone(),
                    branch.clone(),
                    Some(next.graph_revision),
                    next.item_offset,
                    1,
                    next.dependency_offset,
                    1,
                )
                .expect("stale query")
            )
            .await,
        Err(WorkRepositoryError::StaleTaskGraphRevision {
            actual_graph_revision,
            ..
        }) if actual_graph_revision == advanced.parts().current_graph_revision
    ));
    assert!(matches!(
        repository
            .load_task_graph_page(
                WorkTaskGraphQuery::new(
                    WorkOwnerId::parse(id("other-owner")).expect("other owner"),
                    work,
                    branch,
                    None,
                    0,
                    1,
                    0,
                    1,
                )
                .expect("other query")
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    cleanup_owner(&pool, &owner_id).await;
}
