mod common;

use astra_services::{
    runs::{DatabaseRunStateStore, DurableRunRecord, DurableWorkRunBinding, RunStateStore},
    work::{
        DatabaseWorkRepository, GraphRevision, WorkEventKind, WorkEventPageLimit, WorkEventQuery,
        WorkId, WorkOwnerId, WorkRepository, project_pending_runtime_events,
    },
};
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn work_run(
    owner_id: &str,
    run_id: &str,
    session_id: &str,
    work_id: &str,
    branch_id: &str,
) -> DurableRunRecord {
    DurableRunRecord {
        run_id: run_id.to_owned(),
        user_id: owner_id.to_owned(),
        session_id: session_id.to_owned(),
        parent_run_id: None,
        root_run_id: Some(run_id.to_owned()),
        ancestor_path: Some(run_id.to_owned()),
        depth: 0,
        delegation_id: None,
        agent_id: None,
        retry_of: None,
        retry_scope: None,
        status: "running".to_owned(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 0,
        last_event_idx: -1,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        agent_binding_id: None,
        agent_binding_name: None,
        agent_binding_schema_version: None,
        model_offering_id: None,
        resolved_model_name: None,
        runtime_profile: None,
        start_request_fingerprint: None,
        work_binding: Some(DurableWorkRunBinding::new(
            WorkId::parse(work_id).expect("Work id"),
            astra_services::work::WorkBranchId::parse(branch_id).expect("branch id"),
            GraphRevision::INITIAL,
        )),
        events: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("run_display_projections", "user_id"),
        ("run_checkpoints", "user_id"),
        ("agent_run_events", "user_id"),
        ("agent_session_execution_slots", "user_id"),
        ("agent_runs", "user_id"),
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
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn root_run_terminal_fact_is_atomic_bounded_idempotent_and_projected() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("work-event-it");
    let owner_id = id("runtime-event-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let first_run_id = id("run");
    let overflow_run_id = id("run");
    cleanup_owner(&pool, &owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Project durable Run outcomes into Work activity.",
        ))
        .await
        .expect("create Work");
    store
        .insert_run(work_run(
            &owner_id,
            &first_run_id,
            &session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert Work run");

    assert!(
        store
            .update_run_status_with_events_if_current(
                &owner_id,
                &session_id,
                &first_run_id,
                &["running"],
                None,
                "failed",
                None,
                Some("provider connection closed"),
                &[serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"status": "failed"}
                })],
            )
            .await
            .expect("commit terminal Run fact")
    );
    let before_projection: i64 = sqlx::query_scalar(
        "SELECT last_event_seq FROM work_event_sequences WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("event head before projection");
    assert_eq!(
        before_projection, 1,
        "Run truth must not synchronously mutate its projection"
    );

    let projected = project_pending_runtime_events(&pool, 10)
        .await
        .expect("project terminal fact");
    assert!(
        projected.projected >= 1,
        "the global bounded sweep must project this Work's pending terminal fact"
    );
    let page = repository
        .list_events(WorkEventQuery {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("Work"),
            after_event_seq: Some(astra_services::work::WorkEventSeq::INITIAL),
            limit: WorkEventPageLimit::new(10).expect("limit"),
        })
        .await
        .expect("read projected Work event");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, WorkEventKind::RunFailed);
    assert_eq!(
        page.events[0].branch_id.as_ref().map(|id| id.as_str()),
        Some(branch_id.as_str())
    );
    assert_eq!(page.events[0].graph_revision, Some(GraphRevision::INITIAL));
    assert_eq!(
        page.events[0].source_ref.as_str(),
        format!("run:{first_run_id}")
    );

    assert!(
        store
            .update_run_status(&owner_id, &session_id, &first_run_id, "failed", None, None,)
            .await
            .expect("idempotent terminal status replay")
    );
    assert_eq!(
        project_pending_runtime_events(&pool, 10)
            .await
            .expect("project replay")
            .projected,
        0,
        "same terminal fact must not create a second semantic event"
    );

    store
        .insert_run(work_run(
            &owner_id,
            &overflow_run_id,
            &session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert second Work run");
    sqlx::query(
        "UPDATE work_runtime_event_outbox_slots
         SET last_enqueued_event_seq = 1024, last_projected_event_seq = 0, has_pending = 1
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("simulate a projector lagging beyond the bounded ring");
    assert!(
        store
            .update_run_status(
                &owner_id,
                &session_id,
                &overflow_run_id,
                "completed",
                None,
                None,
            )
            .await
            .expect("terminal fact must survive projection overflow")
    );
    assert_eq!(
        store
            .load_run(&owner_id, &overflow_run_id)
            .await
            .expect("load overflow run")
            .expect("run exists")
            .status,
        "completed",
        "derived projection capacity must never falsify an authoritative Run outcome"
    );
    let overflow = project_pending_runtime_events(&pool, 1)
        .await
        .expect("project explicit overflow fact");
    assert_eq!(overflow.coverage_expired, 1);
    let overflow_page = repository
        .list_events(WorkEventQuery {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(&work_id).expect("Work"),
            after_event_seq: Some(page.event_head),
            limit: WorkEventPageLimit::new(10).expect("limit"),
        })
        .await
        .expect("read overflow event");
    assert_eq!(overflow_page.events.len(), 1);
    assert_eq!(
        overflow_page.events[0].kind,
        WorkEventKind::RuntimeEventsExpired
    );
    cleanup_owner(&pool, &owner_id).await;
}
