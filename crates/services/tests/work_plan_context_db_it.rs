mod common;

use astra_services::work::{
    DatabaseWorkRepository, GraphRevision, InternalSessionId, NewWorkItem, WorkBranchId,
    WorkBranchRevision, WorkChangeRef, WorkId, WorkItemEdge, WorkItemEdgeKind, WorkItemId,
    WorkItemKind, WorkItemRevision, WorkItemRevisionRef, WorkItemText, WorkOwnerId, WorkRepository,
    WorkRepositoryError, WorkTaskGraphQuery,
};
use std::sync::Arc;
use tokio::sync::Barrier;

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
        &common::id("intent"),
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

fn plan_proposal(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    branch_revision: WorkBranchRevision,
    graph_revision: GraphRevision,
    additions: Vec<NewWorkItem>,
    dependencies: Vec<WorkItemEdge>,
) -> astra_services::work::NewWorkPlanProposal {
    use astra_services::work::*;
    NewWorkPlanProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        proposal_id: WorkProposalId::parse(common::id("plan-proposal")).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: CriterionSetRevision::INITIAL,
        expected_branch_revision: branch_revision,
        expected_graph_revision: graph_revision,
        additions,
        dependencies,
        revisions: Vec::new(),
        dependency_removals: Vec::new(),
        source_kind: WorkProposalSourceKind::Model,
        source_ref: WorkChangeRef::parse(common::id("context-source")).expect("source"),
        reason: WorkChangeReason::parse("Refine the runtime context").expect("reason"),
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn public_branch_identity_resolves_one_active_owner_scoped_runtime_binding() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
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
        (
            common::id("other-owner"),
            work_id.clone(),
            branch_id.clone(),
        ),
        (
            owner_id.clone(),
            common::id("other-work"),
            branch_id.clone(),
        ),
        (
            owner_id.clone(),
            work_id.clone(),
            common::id("other-branch"),
        ),
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
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn session_item_runtime_binding_accepts_only_the_active_item_in_the_bound_graph() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    let task_id = common::id("task");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            GraphRevision::INITIAL,
            vec![item(&task_id)],
            Vec::new(),
        ))
        .await
        .expect("context proposal");
    repository
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
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
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn session_plan_context_is_bounded_canonical_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    let task_a = common::id("task-a");
    let task_b = common::id("task-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            GraphRevision::INITIAL,
            vec![item(&task_b), item(&task_a)],
            vec![dependency(&task_a, &task_b)],
        ))
        .await
        .expect("context proposal");
    repository
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
        .await
        .expect("graph");

    let context = repository
        .load_plan_context_for_session(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&session_id).expect("session"),
        )
        .await
        .expect("plan context");
    assert_eq!(context.items().len(), 3);
    assert_eq!(context.dependencies(), [dependency(&task_a, &task_b)]);
    assert_eq!(context.items()[0].item_id.as_str(), "root");
    assert_eq!(context.items()[1].item_id.as_str(), task_a);
    assert_eq!(context.items()[2].item_id.as_str(), task_b);
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

    let fork_branch_id = common::id("fork-branch");
    let fork_session_id = common::id("fork-session");
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
    .bind(common::id("fork-cursor"))
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

    let foreign_owner = WorkOwnerId::parse(common::id("foreign-owner")).expect("foreign owner");
    let missing_session = InternalSessionId::parse(common::id("missing-session")).expect("session");
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
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn plan_context_rejects_corrupt_hash_and_missing_item_revision() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    let task_id = common::id("task");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            GraphRevision::INITIAL,
            vec![item(&task_id)],
            Vec::new(),
        ))
        .await
        .expect("context proposal");
    repository
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
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
                &WorkOwnerId::parse(common::id("foreign-owner")).expect("foreign owner"),
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
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn plan_context_racing_graph_advance_is_never_torn() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    let task_id = common::id("task");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");

    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            GraphRevision::INITIAL,
            vec![item(&task_id)],
            Vec::new(),
        ))
        .await
        .expect("context proposal");
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
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
        .await
        .expect("advance graph")
        .resolution
        .expect("graph resolution");
    assert_eq!(
        advanced.result_graph_revision.expect("graph revision"),
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
                assert_eq!(basis.graph_item_count, 2);
                assert_eq!(context.items().len(), 2);
                assert_eq!(context.items()[0].item_id.as_str(), "root");
                assert_eq!(context.items()[1].item_id.as_str(), task_id);
            }
            revision => panic!("unexpected graph revision {revision}"),
        }
        assert!(context.dependencies().is_empty());
    }
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn maximum_active_frontier_uses_one_bounded_item_fetch() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    for batch in (0..255).collect::<Vec<_>>().chunks(64) {
        let basis = repository
            .load(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
            )
            .await
            .expect("current batch basis");
        let proposed = repository
            .propose_plan(astra_services::work::NewWorkPlanProposal {
                owner_id: basis.work.parts().owner_id.clone(),
                work_id: basis.work.parts().work_id.clone(),
                branch_id: basis.delivery_branch.parts().branch_id.clone(),
                proposal_id: astra_services::work::WorkProposalId::parse(common::id(
                    "maximum-proposal",
                ))
                .expect("proposal"),
                expected_work_revision: basis.work.parts().work_revision,
                expected_goal_revision: basis.work.parts().current_goal_revision,
                expected_criteria_set_revision: basis.work.parts().current_criteria_set_revision,
                expected_branch_revision: basis.delivery_branch.parts().branch_revision,
                expected_graph_revision: basis.delivery_branch.parts().current_graph_revision,
                additions: batch
                    .iter()
                    .map(|index| item(&format!("task-{index:03}")))
                    .collect(),
                revisions: Vec::new(),
                dependencies: Vec::new(),
                dependency_removals: Vec::new(),
                source_kind: astra_services::work::WorkProposalSourceKind::Model,
                source_ref: WorkChangeRef::parse(common::id("maximum-frontier")).expect("source"),
                reason: astra_services::work::WorkChangeReason::parse(
                    "Populate the bounded context",
                )
                .expect("reason"),
            })
            .await
            .expect("maximum context batch");
        repository
            .accept_plan_proposal(common::plan_acceptance(
                &proposed,
                &common::id("accept-batch"),
            ))
            .await
            .expect("accept bounded batch");
    }
    let context = repository
        .load_plan_context_for_session(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &InternalSessionId::parse(&session_id).expect("session"),
        )
        .await
        .expect("maximum plan context");
    assert_eq!(context.basis().branch_revision.get(), 5);
    assert_eq!(context.basis().graph_revision.get(), 5);
    assert_eq!(context.basis().graph_item_count, 256);
    assert_eq!(context.items().len(), 256);
    assert_eq!(context.items()[0].item_id.as_str(), "root");
    assert_eq!(context.items()[1].item_id.as_str(), "task-000");
    assert_eq!(context.items()[255].item_id.as_str(), "task-254");
    assert!(context.dependencies().is_empty());
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn public_task_graph_pages_are_owner_scoped_and_fail_closed_across_replan() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");
    let task_a = common::id("task-a");
    let task_b = common::id("task-b");
    let task_c = common::id("task-c");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id, &session_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            WorkBranchRevision::INITIAL,
            GraphRevision::INITIAL,
            vec![item(&task_b), item(&task_a)],
            vec![dependency(&task_a, &task_b)],
        ))
        .await
        .expect("context proposal");
    let graph = repository
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
        .await
        .expect("graph")
        .resolution
        .expect("graph resolution");
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
    assert_eq!(first.items().total, 3);
    assert_eq!(first.dependencies().total, 1);
    assert_eq!(first.items().entries.len(), 1);
    assert_eq!(first.items().entries[0].item_id.as_str(), "root");
    assert_eq!(first.dependencies().entries, [dependency(&task_a, &task_b)]);
    let next = first.next_cursor().cloned().expect("next cursor");
    assert_eq!((next.item_offset, next.dependency_offset), (1, 1));
    assert_eq!(
        next.graph_revision,
        graph.result_graph_revision.expect("graph revision")
    );
    let second = repository
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
    assert_eq!(second.items().entries[0].item_id.as_str(), task_a);
    assert!(second.dependencies().entries.is_empty());
    let last = second.next_cursor().expect("last cursor");
    assert_eq!((last.item_offset, last.dependency_offset), (2, 1));
    let terminal = repository
        .load_task_graph_page(
            WorkTaskGraphQuery::new(
                owner.clone(),
                work.clone(),
                branch.clone(),
                Some(last.graph_revision),
                last.item_offset,
                1,
                last.dependency_offset,
                1,
            )
            .expect("last page query"),
        )
        .await
        .expect("last page");
    assert_eq!(terminal.items().entries[0].item_id.as_str(), task_b);
    assert!(terminal.next_cursor().is_none());
    assert!(terminal.dependencies().entries.is_empty());

    let proposed = repository
        .propose_plan(plan_proposal(
            &owner_id,
            &work_id,
            &branch_id,
            graph.result_branch_revision.expect("branch revision"),
            graph.result_graph_revision.expect("graph revision"),
            vec![item(&task_c)],
            vec![dependency(&task_b, &task_c)],
        ))
        .await
        .expect("context proposal");
    let advanced = repository
        .accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-context"),
        ))
        .await
        .expect("replan")
        .resolution
        .expect("graph resolution");
    let fresh = repository
        .load_task_graph_page(
            WorkTaskGraphQuery::new(
                owner.clone(),
                work.clone(),
                branch.clone(),
                None,
                0,
                8,
                0,
                128,
            )
            .expect("fresh query"),
        )
        .await
        .expect("fresh graph page");
    assert_eq!(
        fresh
            .items()
            .entries
            .iter()
            .map(|item| item.item_id.as_str())
            .collect::<Vec<_>>(),
        ["root", task_a.as_str(), task_b.as_str(), task_c.as_str()]
    );
    assert_eq!(
        fresh.dependencies().entries,
        [dependency(&task_a, &task_b), dependency(&task_b, &task_c)]
    );
    assert!(fresh.next_cursor().is_none());
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
        }) if actual_graph_revision == advanced.result_graph_revision.expect("graph revision")
    ));
    assert!(matches!(
        repository
            .load_task_graph_page(
                WorkTaskGraphQuery::new(
                    WorkOwnerId::parse(common::id("other-owner")).expect("other owner"),
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
    common::cleanup_work_owner(&pool, &owner_id).await;
}
