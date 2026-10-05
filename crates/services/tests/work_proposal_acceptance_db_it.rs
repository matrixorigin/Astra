mod common;

use astra_services::work::{
    DatabaseWorkRepository, GoalRevision, GraphRevision, NewWorkItem, NewWorkPlanProposal,
    WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkContentHash, WorkId,
    WorkItemDeclarationState, WorkItemEdge, WorkItemEdgeKind, WorkItemId, WorkItemKind,
    WorkItemRevision, WorkItemRevisionChange, WorkItemText, WorkOwnerId, WorkProposalId,
    WorkProposalSourceKind, WorkProposalStatus, WorkRepository, WorkRepositoryError, WorkRevision,
};
use sqlx::Row;

fn genesis(owner_id: &str, work_id: &str, branch_id: &str) -> astra_services::work::WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        &common::id("session"),
        &common::id("intent"),
        "Expand a typed plan without changing authority.",
    )
}

fn item(item_id: &str) -> NewWorkItem {
    NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item"),
        kind: WorkItemKind::Task,
        objective: WorkItemText::parse(format!("Implement {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("{item_id} has exact verification"))
            .expect("expected result"),
    }
}

fn dependency(predecessor: &str, successor: &str) -> WorkItemEdge {
    WorkItemEdge {
        predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
        successor_item_id: WorkItemId::parse(successor).expect("successor"),
        kind: WorkItemEdgeKind::Dependency,
    }
}

fn proposal(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    proposal_id: &str,
    additions: Vec<NewWorkItem>,
    dependencies: Vec<WorkItemEdge>,
) -> NewWorkPlanProposal {
    NewWorkPlanProposal {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        proposal_id: WorkProposalId::parse(proposal_id).expect("proposal"),
        expected_work_revision: WorkRevision::INITIAL,
        expected_goal_revision: GoalRevision::INITIAL,
        expected_criteria_set_revision: astra_services::work::CriterionSetRevision::INITIAL,
        expected_branch_revision: WorkBranchRevision::INITIAL,
        expected_graph_revision: GraphRevision::INITIAL,
        additions,
        revisions: Vec::new(),
        dependencies,
        dependency_removals: Vec::new(),
        reason: astra_services::work::WorkChangeReason::parse("Refine the Work plan")
            .expect("reason"),
        source_kind: WorkProposalSourceKind::Model,
        source_ref: WorkChangeRef::parse(common::id("model-invocation")).expect("source"),
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_acceptance_materializes_one_graph_revision_and_is_exactly_idempotent() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let proposal_id = common::id("proposal");
    let root_task = common::id("root-task");
    let task_a = common::id("task-a");
    let task_b = common::id("task-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let root_proposal = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &common::id("root-proposal"),
            vec![item(&root_task)],
            Vec::new(),
        ))
        .await
        .expect("root proposal");
    let rooted = repository
        .accept_plan_proposal(common::plan_acceptance(
            &root_proposal,
            &common::id("accept-root"),
        ))
        .await
        .expect("materialize root task")
        .resolution
        .expect("root resolution");
    let mut plan = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &proposal_id,
        vec![item(&task_b), item(&task_a)],
        vec![
            dependency(&root_task, &task_a),
            dependency(&task_a, &task_b),
        ],
    );
    plan.expected_branch_revision = rooted.result_branch_revision.expect("root branch");
    plan.expected_graph_revision = rooted.result_graph_revision.expect("root graph");
    let proposed = repository.propose_plan(plan).await.expect("proposal");
    let command = common::plan_acceptance(&proposed, &common::id("root-action"));
    let mut wrong_hash = command.clone();
    wrong_hash.payload_hash =
        WorkContentHash::parse(format!("sha256:{}", "f".repeat(64))).expect("different hash");
    assert!(matches!(
        repository.accept_plan_proposal(wrong_hash).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::ProposalPayloadHash
        })
    ));
    let (left, right) = tokio::join!(
        repository.accept_plan_proposal(command.clone()),
        repository.accept_plan_proposal(command.clone())
    );
    let left = left.expect("first acceptance");
    let right = right.expect("concurrent exact retry");
    assert_eq!(left, right);
    assert_eq!(left.status, WorkProposalStatus::Accepted);
    let resolution = left.resolution.as_ref().expect("accepted resolution");
    assert_eq!(
        resolution.result_branch_revision,
        Some(WorkBranchRevision::new(3).expect("branch revision"))
    );
    assert_eq!(
        resolution.result_graph_revision,
        Some(GraphRevision::new(3).expect("graph revision"))
    );

    let loaded = repository
        .load(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
        )
        .await
        .expect("accepted Work");
    assert_eq!(
        loaded.delivery_branch.parts().branch_revision,
        resolution.result_branch_revision.expect("branch result")
    );
    assert_eq!(
        loaded.delivery_branch.parts().current_graph_revision,
        resolution.result_graph_revision.expect("graph result")
    );
    let graph = sqlx::query(
        "SELECT parent_revision, patch_ref, patch_hash, actor_kind, actor_id,
                item_count, edge_count
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 3",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("accepted graph revision");
    assert_eq!(graph.try_get::<i64, _>("parent_revision").unwrap(), 2);
    assert_eq!(
        graph.try_get::<String, _>("patch_ref").unwrap(),
        proposal_id
    );
    assert_eq!(
        graph.try_get::<String, _>("patch_hash").unwrap(),
        proposed.payload_hash.as_str()
    );
    assert_eq!(graph.try_get::<String, _>("actor_kind").unwrap(), "model");
    assert_eq!(graph.try_get::<String, _>("actor_id").unwrap(), proposal_id);
    assert_eq!(graph.try_get::<i64, _>("item_count").unwrap(), 4);
    assert_eq!(graph.try_get::<i64, _>("edge_count").unwrap(), 2);
    for (table, expected) in [
        ("work_items", 4_i64),
        ("work_item_revisions", 4_i64),
        ("work_item_edges", 2_i64),
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE owner_id = ? AND work_id = ?"
        ))
        .bind(&owner_id)
        .bind(&work_id)
        .fetch_one(pool.get())
        .await
        .unwrap_or_else(|error| panic!("count {table}: {error}"));
        assert_eq!(count, expected, "unexpected durable {table} rows");
    }
    let event_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM work_events
         WHERE owner_id = ? AND work_id = ? ORDER BY event_seq",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("events");
    assert_eq!(
        event_kinds,
        [
            "work_created",
            "plan_proposed",
            "graph_replaced",
            "plan_proposed",
            "graph_replaced"
        ]
    );

    let mut later = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("later-proposal"),
        vec![item(&common::id("later-task"))],
        Vec::new(),
    );
    later.expected_branch_revision = WorkBranchRevision::new(3).expect("branch r3");
    later.expected_graph_revision = GraphRevision::new(3).expect("graph r3");
    let later = repository
        .propose_plan(later)
        .await
        .expect("later proposal");
    repository
        .accept_plan_proposal(common::plan_acceptance(&later, &common::id("accept-later")))
        .await
        .expect("later branch advance");
    assert_eq!(
        repository
            .accept_plan_proposal(command.clone())
            .await
            .expect("accepted retry after later branch advance"),
        left,
        "idempotent acceptance returns its original immutable result"
    );

    let different_resolution =
        common::plan_acceptance(&proposed, &common::id("different-root-action"));
    assert!(matches!(
        repository.accept_plan_proposal(different_resolution).await,
        Err(WorkRepositoryError::WorkProposalAlreadyResolved {
            status: WorkProposalStatus::Accepted
        })
    ));
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn replan_creates_successor_item_revisions_and_removes_dependencies_atomically() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let task_a = common::id("task-a");
    let task_b = common::id("task-b");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");

    let first = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &common::id("initial-plan"),
            vec![item(&task_a), item(&task_b)],
            vec![dependency(&task_a, &task_b)],
        ))
        .await
        .expect("initial proposal");
    let first = repository
        .accept_plan_proposal(common::plan_acceptance(
            &first,
            &common::id("initial-acceptance"),
        ))
        .await
        .expect("initial acceptance");
    let first_resolution = first.resolution.expect("initial resolution");

    let revised_a_objective = "Implement task A after the observed constraint changed";
    let mut replan = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("replan"),
        Vec::new(),
        Vec::new(),
    );
    replan.expected_branch_revision = first_resolution
        .result_branch_revision
        .expect("initial branch revision");
    replan.expected_graph_revision = first_resolution
        .result_graph_revision
        .expect("initial graph revision");
    replan.revisions = vec![
        WorkItemRevisionChange::new(
            WorkItemId::parse(&task_a).expect("task A"),
            WorkItemRevision::INITIAL,
            WorkItemKind::Task,
            WorkItemText::parse(revised_a_objective).expect("objective"),
            WorkItemText::parse("Task A satisfies the revised constraint")
                .expect("expected result"),
            WorkItemDeclarationState::Active,
        ),
        WorkItemRevisionChange::new(
            WorkItemId::parse(&task_b).expect("task B"),
            WorkItemRevision::INITIAL,
            WorkItemKind::Task,
            item(&task_b).objective,
            item(&task_b).expected_result,
            WorkItemDeclarationState::Cancelled,
        ),
    ];
    replan.dependency_removals = vec![dependency(&task_a, &task_b)];
    replan.reason = astra_services::work::WorkChangeReason::parse(
        "Observed constraints make task B obsolete and change task A",
    )
    .expect("reason");
    let replan = repository
        .propose_plan(replan)
        .await
        .expect("replan proposal");
    let accepted = repository
        .accept_plan_proposal(common::plan_acceptance(
            &replan,
            &common::id("replan-acceptance"),
        ))
        .await
        .expect("replan acceptance");
    let result_graph_revision = accepted
        .resolution
        .as_ref()
        .and_then(|resolution| resolution.result_graph_revision)
        .expect("replan graph revision");

    let revisions = sqlx::query(
        "SELECT item_id, revision, objective, declaration_state
         FROM work_item_revisions
         WHERE owner_id = ? AND work_id = ? AND item_id IN (?, ?)
         ORDER BY item_id, revision",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&task_a)
    .bind(&task_b)
    .fetch_all(pool.get())
    .await
    .expect("item revision history");
    assert_eq!(revisions.len(), 4, "both immutable predecessors remain");
    let revised_a = revisions
        .iter()
        .find(|row| {
            row.try_get::<String, _>("item_id").unwrap() == task_a
                && row.try_get::<i64, _>("revision").unwrap() == 2
        })
        .expect("task A revision 2");
    assert_eq!(
        revised_a.try_get::<String, _>("objective").unwrap(),
        revised_a_objective
    );
    let cancelled_b = revisions
        .iter()
        .find(|row| {
            row.try_get::<String, _>("item_id").unwrap() == task_b
                && row.try_get::<i64, _>("revision").unwrap() == 2
        })
        .expect("task B revision 2");
    assert_eq!(
        cancelled_b
            .try_get::<String, _>("declaration_state")
            .unwrap(),
        "cancelled"
    );

    let graph = sqlx::query(
        "SELECT item_revision_manifest_json, edge_count, reason
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(result_graph_revision.get())
    .fetch_one(pool.get())
    .await
    .expect("replanned graph");
    let manifest: serde_json::Value = serde_json::from_str(
        &graph
            .try_get::<String, _>("item_revision_manifest_json")
            .unwrap(),
    )
    .expect("manifest JSON");
    assert!(
        manifest
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["item_id"] == task_a && item["revision"] == 2 })
    );
    assert!(
        manifest
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["item_id"] == task_b && item["revision"] == 2 })
    );
    assert_eq!(graph.try_get::<i64, _>("edge_count").unwrap(), 0);
    assert_eq!(
        graph.try_get::<String, _>("reason").unwrap(),
        "Observed constraints make task B obsolete and change task A"
    );

    let proposal_count_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal count");
    let mut stale_item_basis = proposal(
        &owner_id,
        &work_id,
        &branch_id,
        &common::id("stale-replan"),
        Vec::new(),
        Vec::new(),
    );
    stale_item_basis.expected_branch_revision = accepted
        .resolution
        .as_ref()
        .and_then(|resolution| resolution.result_branch_revision)
        .expect("current branch revision");
    stale_item_basis.expected_graph_revision = result_graph_revision;
    stale_item_basis.revisions = vec![WorkItemRevisionChange::new(
        WorkItemId::parse(&task_a).expect("task A"),
        WorkItemRevision::INITIAL,
        WorkItemKind::Task,
        WorkItemText::parse("Stale overwrite").expect("objective"),
        WorkItemText::parse("Must not persist").expect("result"),
        WorkItemDeclarationState::Active,
    )];
    assert!(matches!(
        repository.propose_plan(stale_item_basis).await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::WorkItemRevision
        })
    ));
    let proposal_count_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("proposal count after stale rejection");
    assert_eq!(proposal_count_after, proposal_count_before);
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn sibling_branches_allocate_distinct_successors_from_one_item_revision() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let delivery_branch = common::id("delivery");
    let sibling_branch = common::id("sibling");
    let sibling_session = common::id("session");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &delivery_branch))
        .await
        .expect("genesis");
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, project_retention_policy)
         VALUES (?, ?, 'active', 0, 'session')",
    )
    .bind(&sibling_session)
    .bind(&owner_id)
    .execute(pool.get())
    .await
    .expect("sibling session");
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id,
          origin_branch_id, fork_cursor, goal_revision_ref, criteria_set_revision_ref,
          basis_graph_revision, current_graph_revision)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, 'fork:root',
                goal_revision_ref, criteria_set_revision_ref,
                current_graph_revision, current_graph_revision
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&sibling_branch)
    .bind(&sibling_session)
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&delivery_branch)
    .execute(pool.get())
    .await
    .expect("sibling branch");

    // The explicit sibling fixture needs the proposal admission row that
    // production branch creation installs alongside work_branches.
    sqlx::query(
        "INSERT INTO work_proposal_sequences (owner_id, work_id, branch_id, last_proposal_seq) VALUES (?, ?, ?, 0)",
    ).bind(&owner_id).bind(&work_id).bind(&sibling_branch)
        .execute(pool.get()).await.expect("sibling proposal sequence");

    let replan = |branch_id: &str, objective: &str| {
        let mut plan = proposal(
            &owner_id,
            &work_id,
            branch_id,
            &common::id("replan"),
            Vec::new(),
            Vec::new(),
        );
        plan.revisions.push(WorkItemRevisionChange::new(
            WorkItemId::root(),
            WorkItemRevision::INITIAL,
            WorkItemKind::Milestone,
            WorkItemText::parse(objective).expect("objective"),
            WorkItemText::parse("The branch-specific plan is explicit").expect("result"),
            WorkItemDeclarationState::Active,
        ));
        plan
    };
    let delivery_proposal = repository
        .propose_plan(replan(&delivery_branch, "Explore the delivery approach"))
        .await
        .expect("delivery proposal");
    let sibling_proposal = repository
        .propose_plan(replan(&sibling_branch, "Explore the sibling approach"))
        .await
        .expect("sibling proposal");
    let (delivery, sibling) = tokio::join!(
        repository.accept_plan_proposal(common::plan_acceptance(
            &delivery_proposal,
            &common::id("accept-delivery")
        )),
        repository.accept_plan_proposal(common::plan_acceptance(
            &sibling_proposal,
            &common::id("accept-sibling")
        )),
    );
    let delivery = delivery
        .expect("delivery replan")
        .resolution
        .expect("delivery resolution");
    let sibling = sibling
        .expect("sibling replan")
        .resolution
        .expect("sibling resolution");
    assert_ne!(
        delivery.result_graph_revision,
        sibling.result_graph_revision
    );

    let rows = sqlx::query(
        "SELECT revision, parent_revision, objective
         FROM work_item_revisions
         WHERE owner_id = ? AND work_id = ? AND item_id = 'root' AND revision > 1
         ORDER BY revision",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("sibling item revisions");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].try_get::<i64, _>("revision").unwrap(), 2);
    assert_eq!(rows[1].try_get::<i64, _>("revision").unwrap(), 3);
    assert!(
        rows.iter()
            .all(|row| { row.try_get::<Option<i64>, _>("parent_revision").unwrap() == Some(1) })
    );
    let objectives = rows
        .iter()
        .map(|row| row.try_get::<String, _>("objective").unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        objectives,
        std::collections::BTreeSet::from([
            "Explore the delivery approach".to_string(),
            "Explore the sibling approach".to_string(),
        ])
    );
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn stale_and_expired_acceptance_never_leave_graph_residue() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &common::id("proposal"),
            vec![item(&common::id("task"))],
            Vec::new(),
        ))
        .await
        .expect("proposal");
    let criteria = repository
        .propose_criteria(astra_services::work::NewWorkCriteriaProposal {
            owner_id: proposed.proposal.owner_id.clone(),
            work_id: proposed.proposal.work_id.clone(),
            branch_id: proposed.proposal.branch_id.clone(),
            proposal_id: WorkProposalId::parse(common::id("criteria-proposal")).expect("proposal"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            expected_criteria_set_revision: astra_services::work::CriterionSetRevision::INITIAL,
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            members: vec![astra_services::work::WorkCriteriaProposalMember::New {
                criterion_id: astra_services::work::CriterionId::parse(common::id("criterion"))
                    .expect("criterion"),
                definition: astra_services::work::CriterionDefinition::HumanReview {
                    statement: astra_services::work::CriterionStatement::parse(
                        "A reviewer accepts the result",
                    )
                    .expect("statement"),
                },
            }],
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse(common::id("criteria-source")).expect("source"),
        })
        .await
        .expect("criteria proposal");
    repository
        .accept_criteria_proposal(common::criteria_acceptance(
            &criteria,
            &common::id("accept-criteria"),
        ))
        .await
        .expect("advance Work criteria basis");
    assert!(matches!(
        repository
            .accept_plan_proposal(common::plan_acceptance(
                &proposed,
                &common::id("root-action")
            ))
            .await,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::WorkRevision
        })
    ));
    let state = sqlx::query(
        "SELECT p.status, gs.last_revision, b.branch_revision, b.current_graph_revision,
                (SELECT COUNT(*) FROM work_items wi
                 WHERE wi.owner_id = p.owner_id AND wi.work_id = p.work_id) AS item_count
         FROM work_proposals p
         JOIN work_graph_sequences gs ON gs.owner_id = p.owner_id AND gs.work_id = p.work_id
         JOIN work_branches b ON b.owner_id = p.owner_id AND b.work_id = p.work_id
          AND b.branch_id = p.branch_id
         WHERE p.owner_id = ? AND p.work_id = ? AND p.proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(proposed.proposal.proposal_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("rolled back state");
    assert_eq!(state.try_get::<String, _>("status").unwrap(), "pending");
    assert_eq!(state.try_get::<i64, _>("last_revision").unwrap(), 1);
    assert_eq!(state.try_get::<i64, _>("branch_revision").unwrap(), 1);
    assert_eq!(
        state.try_get::<i64, _>("current_graph_revision").unwrap(),
        1
    );
    assert_eq!(state.try_get::<i64, _>("item_count").unwrap(), 1);

    sqlx::query(
        "UPDATE work_proposals
         SET proposed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY),
             expires_at = DATE_SUB(NOW(6), INTERVAL 1 DAY)
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(proposed.proposal.proposal_id.as_str())
    .execute(pool.get())
    .await
    .expect("expire pending proposal");
    assert!(matches!(
        repository
            .accept_plan_proposal(common::plan_acceptance(
                &proposed,
                &common::id("expired-action")
            ))
            .await,
        Err(WorkRepositoryError::WorkProposalAlreadyResolved {
            status: WorkProposalStatus::Expired
        })
    ));
    let expired = sqlx::query(
        "SELECT status, resolution_ref, result_branch_revision, result_graph_revision
         FROM work_proposals WHERE owner_id = ? AND work_id = ? AND proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(proposed.proposal.proposal_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("expired proposal");
    assert_eq!(expired.try_get::<String, _>("status").unwrap(), "expired");
    assert_eq!(
        expired.try_get::<String, _>("resolution_ref").unwrap(),
        "proposal-retention-expiry"
    );
    assert_eq!(
        expired
            .try_get::<Option<i64>, _>("result_branch_revision")
            .unwrap(),
        None
    );
    assert_eq!(
        expired
            .try_get::<Option<i64>, _>("result_graph_revision")
            .unwrap(),
        None
    );
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn acceptance_event_conflict_rolls_back_graph_items_branch_and_proposal() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let proposal_id = common::id("proposal");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &proposal_id,
            vec![item(&common::id("task"))],
            Vec::new(),
        ))
        .await
        .expect("proposal");
    sqlx::query(
        "INSERT INTO work_events
         (owner_id, work_id, event_seq, branch_id, event_kind,
          branch_revision, graph_revision, source_ref)
         VALUES (?, ?, 9000, ?, 'graph_replaced', 2, 2, ?)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&proposal_id)
    .execute(pool.get())
    .await
    .expect("inject graph event identity conflict");
    assert!(matches!(
        repository
            .accept_plan_proposal(common::plan_acceptance(
                &proposed,
                &common::id("root-action")
            ))
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: astra_services::work::WorkConflictResource::WorkEventIdentity
        })
    ));
    let state = sqlx::query(
        "SELECT p.status, p.resolution_ref, gs.last_revision,
                b.branch_revision, b.current_graph_revision,
                (SELECT COUNT(*) FROM work_items wi
                 WHERE wi.owner_id = p.owner_id AND wi.work_id = p.work_id) AS item_count,
                es.last_event_seq
         FROM work_proposals p
         JOIN work_graph_sequences gs ON gs.owner_id = p.owner_id AND gs.work_id = p.work_id
         JOIN work_branches b ON b.owner_id = p.owner_id AND b.work_id = p.work_id
          AND b.branch_id = p.branch_id
         JOIN work_event_sequences es ON es.owner_id = p.owner_id AND es.work_id = p.work_id
         WHERE p.owner_id = ? AND p.work_id = ? AND p.proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .fetch_one(pool.get())
    .await
    .expect("rolled back state");
    assert_eq!(state.try_get::<String, _>("status").unwrap(), "pending");
    assert_eq!(
        state
            .try_get::<Option<String>, _>("resolution_ref")
            .unwrap(),
        None
    );
    assert_eq!(state.try_get::<i64, _>("last_revision").unwrap(), 1);
    assert_eq!(state.try_get::<i64, _>("branch_revision").unwrap(), 1);
    assert_eq!(
        state.try_get::<i64, _>("current_graph_revision").unwrap(),
        1
    );
    assert_eq!(state.try_get::<i64, _>("item_count").unwrap(), 1);
    assert_eq!(state.try_get::<i64, _>("last_event_seq").unwrap(), 2);
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn competing_proposal_acceptances_have_one_complete_winner() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let proposal_id = common::id("proposal");
    let proposed_task = common::id("proposed-task");
    let competing_task = common::id("competing-task");
    repository
        .create_genesis(genesis(&owner_id, &work_id, &branch_id))
        .await
        .expect("genesis");
    let proposed = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &proposal_id,
            vec![item(&proposed_task)],
            Vec::new(),
        ))
        .await
        .expect("proposal");
    let competing = repository
        .propose_plan(proposal(
            &owner_id,
            &work_id,
            &branch_id,
            &common::id("competing-proposal"),
            vec![item(&competing_task)],
            Vec::new(),
        ))
        .await
        .expect("competing proposal");
    let (accepted, other) = tokio::join!(
        repository.accept_plan_proposal(common::plan_acceptance(
            &proposed,
            &common::id("accept-first")
        )),
        repository.accept_plan_proposal(common::plan_acceptance(
            &competing,
            &common::id("accept-competing")
        ))
    );
    let acceptance_won = accepted.is_ok();
    assert_ne!(acceptance_won, other.is_ok(), "exactly one proposal wins");
    let loser = if acceptance_won { other } else { accepted };
    assert!(matches!(
        loser,
        Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: astra_services::work::WorkProposalBasisResource::BranchRevision
        })
    ));
    for (recorded, won) in [(&proposed, acceptance_won), (&competing, !acceptance_won)] {
        let stored = repository
            .load_plan_proposal(
                &recorded.proposal.owner_id,
                &recorded.proposal.work_id,
                &recorded.proposal.proposal_id,
            )
            .await
            .expect("reload proposal")
            .expect("recorded proposal");
        assert_eq!(
            stored.status,
            if won {
                WorkProposalStatus::Accepted
            } else {
                WorkProposalStatus::Pending
            }
        );
        assert_eq!(stored.resolution.is_some(), won);
    }
    let state = sqlx::query(
        "SELECT p.status, gs.last_revision, b.branch_revision, b.current_graph_revision,
                (SELECT COUNT(*) FROM work_graph_revisions gr
                 WHERE gr.owner_id = p.owner_id AND gr.work_id = p.work_id) AS graph_count,
                (SELECT COUNT(*) FROM work_items wi
                 WHERE wi.owner_id = p.owner_id AND wi.work_id = p.work_id) AS item_count,
                es.last_event_seq
         FROM work_proposals p
         JOIN work_graph_sequences gs ON gs.owner_id = p.owner_id AND gs.work_id = p.work_id
         JOIN work_branches b ON b.owner_id = p.owner_id AND b.work_id = p.work_id
          AND b.branch_id = p.branch_id
         JOIN work_event_sequences es ON es.owner_id = p.owner_id AND es.work_id = p.work_id
         WHERE p.owner_id = ? AND p.work_id = ? AND p.proposal_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&proposal_id)
    .fetch_one(pool.get())
    .await
    .expect("single-winner state");
    assert_eq!(state.try_get::<i64, _>("last_revision").unwrap(), 2);
    assert_eq!(state.try_get::<i64, _>("branch_revision").unwrap(), 2);
    assert_eq!(
        state.try_get::<i64, _>("current_graph_revision").unwrap(),
        2
    );
    assert_eq!(state.try_get::<i64, _>("graph_count").unwrap(), 2);
    assert_eq!(state.try_get::<i64, _>("item_count").unwrap(), 2);
    assert_eq!(state.try_get::<i64, _>("last_event_seq").unwrap(), 4);
    assert_eq!(
        state.try_get::<String, _>("status").unwrap(),
        if acceptance_won {
            "accepted"
        } else {
            "pending"
        }
    );
    let persisted_items: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM work_items WHERE owner_id = ? AND work_id = ? ORDER BY item_id",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_all(pool.get())
    .await
    .expect("materialized items");
    assert_eq!(
        persisted_items
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "root".to_string(),
            if acceptance_won {
                proposed_task
            } else {
                competing_task
            },
        ])
    );
    common::cleanup_work_owner(&pool, &owner_id).await;
}
