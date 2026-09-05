//! MatrixOne-backed integration tests for edge dispatch and sweeper leases.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test edge_dispatch_db_it -- --ignored --test-threads=1

mod common;

use std::sync::Arc;

use astra_services::multi_agent::{
    DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, EdgeDispatchAdmission,
    EdgeDispatchAdmissionError, EdgeDispatchIdentity, EdgeDispatchService, EdgeRegistrationLease,
    EdgeRegistryService,
};
use sqlx::Row;
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────

fn unique_suffix() -> String {
    Uuid::new_v4().to_string()
}

fn dispatch_identity(user_id: &str, request_id: &str) -> EdgeDispatchIdentity {
    EdgeDispatchIdentity::new(
        user_id,
        format!("sess-{request_id}"),
        format!("run-{request_id}"),
        format!("chain-{request_id}"),
        request_id,
    )
}

/// All online tests require the env var gate.
fn require_env() {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
}

type RegistryPrivacyState = (
    String,
    i8,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn registry_privacy_state(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    edge_agent_id: &str,
) -> RegistryPrivacyState {
    sqlx::query_as(
        "SELECT edge_id, registration_state, registration_claim_id, \
                hostname, worktree_path, capabilities_json, workspace_id \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(user_id)
    .bind(edge_agent_id)
    .fetch_one(pool)
    .await
    .expect("read edge registry privacy state")
}

async fn publish_registry_predecessor(
    service: &DatabaseEdgeRegistryService,
    user_id: &str,
    edge_agent_id: &str,
) -> EdgeRegistrationLease {
    let lease = service
        .register_or_update_with_lease(
            user_id,
            edge_agent_id,
            "edge-old",
            Some("old-private-host"),
            Some("/old/private/worktree"),
            Some(serde_json::json!({"generation": "old"})),
            Some("workspace-old"),
        )
        .await
        .expect("claim predecessor");
    assert!(
        service
            .finalize_registration(&lease)
            .await
            .expect("finalize predecessor")
    );
    assert!(
        service
            .release_registration(&lease)
            .await
            .expect("publish predecessor")
    );
    lease
}

async fn claim_registry_successor(
    service: &DatabaseEdgeRegistryService,
    user_id: &str,
    edge_agent_id: &str,
) -> EdgeRegistrationLease {
    service
        .register_or_update_with_lease(
            user_id,
            edge_agent_id,
            "edge-new",
            Some("new-host"),
            Some("/new/worktree"),
            Some(serde_json::json!({"generation": "new"})),
            Some("workspace-new"),
        )
        .await
        .expect("claim successor")
}

// ═══════════════════════════════════════════════════════════════════════
// DatabaseEdgeDispatchService
// ═══════════════════════════════════════════════════════════════════════

/// Insert → poll returns pending → mark_dispatched → poll returns empty → deliver_result → completed.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_full_lifecycle() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-usr-{}", unique_suffix());
    let agent_id = format!("ed-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_id, &request_id);

    // 1. Insert dispatch
    let payload = r#"{"tool":"bash","args":{"command":"echo hi"}}"#;
    svc.insert_dispatch(&identity, &agent_id, payload)
        .await
        .expect("insert_dispatch");

    // 2. Poll atomically claims and marks as dispatched within a transaction
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending");
    assert_eq!(rows.len(), 1, "should have 1 pending dispatch");
    assert_eq!(rows[0].request_id, request_id);

    // 3. Poll again — now empty (row was claimed in step 2)
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending after claim");
    assert!(rows.is_empty(), "should be empty after claiming");

    // 4. Deliver result (with edge_agent_id for auth)
    let ok = svc
        .deliver_result(&identity, &agent_id, r#"{"output":"hello"}"#)
        .await
        .expect("deliver_result");
    assert!(ok, "deliver_result should return true for existing request");

    // 6. wait_result returns the result
    let result = svc
        .wait_result(&identity, std::time::Duration::from_secs(5))
        .await
        .expect("wait_result");
    assert!(
        result.is_some(),
        "wait_result should return Some with result json"
    );
    assert!(
        result.unwrap().contains("hello"),
        "result should contain hello"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_admission_replays_terminal_and_rejects_identity_conflicts() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user_id = format!("ed-admit-usr-{}", unique_suffix());
    let agent_id = format!("ed-admit-agent-{}", unique_suffix());
    let identity = dispatch_identity(&user_id, &Uuid::new_v4().to_string());
    let payload = r#"{"tool":"bash","args":{"command":"echo once"}}"#;

    assert_eq!(
        svc.admit_dispatch(&identity, &agent_id, payload)
            .await
            .expect("first admission"),
        EdgeDispatchAdmission::Pending
    );
    assert!(matches!(
        svc.admit_dispatch(&identity, "different-agent", payload)
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(message)) if message.contains("conflicts")
    ));
    assert!(matches!(
        svc.admit_dispatch(&identity, &agent_id, r#"{"tool":"different"}"#)
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(message)) if message.contains("conflicts")
    ));
    assert!(
        svc.claim_direct_dispatch(&identity, &agent_id)
            .await
            .expect("first direct claim")
    );
    assert!(
        !svc.claim_direct_dispatch(&identity, &agent_id)
            .await
            .expect("duplicate direct claim"),
        "only one delivery path may own the durable dispatch boundary"
    );

    let result_json = r#"{"status":"completed","output":"once"}"#;
    assert!(
        svc.deliver_result(&identity, &agent_id, result_json)
            .await
            .expect("terminal result")
    );
    let EdgeDispatchAdmission::Terminal(replayed_json) = svc
        .admit_dispatch(&identity, &agent_id, payload)
        .await
        .expect("terminal replay admission")
    else {
        panic!("terminal identity must replay its durable result");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&replayed_json)
            .expect("persisted terminal result must remain valid JSON"),
        serde_json::from_str::<serde_json::Value>(result_json)
            .expect("fixture terminal result must be valid JSON"),
        "durable replay preserves JSON meaning independently of object-key storage order"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_batched_wait_converges_across_pods_under_concurrency() {
    require_env();
    let pool = common::setup_pool().await;
    let producer = Arc::new(DatabaseEdgeDispatchService::new(pool.get().clone()));
    let waiter = Arc::new(DatabaseEdgeDispatchService::new(pool.get().clone()));
    let user_id = format!("ed-batch-usr-{}", unique_suffix());
    let agent_id = format!("ed-batch-agent-{}", unique_suffix());
    let identities = (0..129)
        .map(|index| dispatch_identity(&user_id, &format!("batch-{index}-{}", unique_suffix())))
        .collect::<Vec<_>>();

    let mut inserts = tokio::task::JoinSet::new();
    for identity in identities.clone() {
        let producer = producer.clone();
        let agent_id = agent_id.clone();
        inserts.spawn(async move { producer.insert_dispatch(&identity, &agent_id, "{}").await });
    }
    while let Some(insert) = inserts.join_next().await {
        insert.unwrap().expect("concurrent dispatch insert");
    }

    let mut waits = tokio::task::JoinSet::new();
    for identity in identities.clone() {
        let waiter = waiter.clone();
        waits.spawn(async move {
            waiter
                .wait_result(&identity, std::time::Duration::from_secs(10))
                .await
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let mut completions = tokio::task::JoinSet::new();
    for identity in identities {
        let producer = producer.clone();
        let agent_id = agent_id.clone();
        completions.spawn(async move {
            producer
                .deliver_result(
                    &identity,
                    &agent_id,
                    &serde_json::json!({
                        "status": "completed",
                        "output": identity.request_id.clone(),
                    })
                    .to_string(),
                )
                .await
        });
    }
    while let Some(completion) = completions.join_next().await {
        assert!(completion.unwrap().expect("concurrent dispatch completion"));
    }

    let mut resolved = 0;
    while let Some(wait) = waits.join_next().await {
        assert!(wait.unwrap().expect("batched wait_result").is_some());
        resolved += 1;
    }
    assert_eq!(resolved, 129);
}

/// deliver_result for unknown request_id returns false.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_deliver_result_nonexistent_returns_false() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user_id = format!("ed-miss-usr-{}", unique_suffix());
    let identity = dispatch_identity(&user_id, "no-such-request");

    let ok = svc
        .deliver_result(&identity, "any-agent", "{}")
        .await
        .expect("deliver_result");
    assert!(
        !ok,
        "deliver_result for nonexistent request should return false"
    );
}

/// deliver_result with wrong edge_agent_id must return false (security boundary).
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_deliver_result_wrong_agent_rejected() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-usr-{}", unique_suffix());
    let agent_id = format!("ed-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_id, &request_id);

    // Insert dispatch with agent_id
    svc.insert_dispatch(&identity, &agent_id, r#"{"test":true}"#)
        .await
        .expect("insert_dispatch");

    // Try to deliver with a DIFFERENT agent — must be rejected
    let ok = svc
        .deliver_result(&identity, "wrong-agent-id", r#"{"output":"stolen"}"#)
        .await
        .expect("deliver_result");
    assert!(
        !ok,
        "deliver_result with wrong agent_id MUST return false — cross-agent injection"
    );

    // Verify the original agent can still deliver
    let ok = svc
        .deliver_result(&identity, &agent_id, r#"{"output":"legit"}"#)
        .await
        .expect("deliver_result");
    assert!(
        ok,
        "correct agent_id should succeed after wrong agent rejection"
    );
}

/// fail_dispatch must enforce the same durable owner boundary as result delivery.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_fail_wrong_agent_cannot_terminate_dispatch() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user_id = format!("ed-fail-owner-usr-{}", unique_suffix());
    let agent_id = format!("ed-fail-owner-agent-{}", unique_suffix());
    let identity = dispatch_identity(&user_id, &Uuid::new_v4().to_string());

    svc.insert_dispatch(&identity, &agent_id, r#"{"test":true}"#)
        .await
        .expect("insert dispatch");

    assert!(
        !svc.fail_dispatch(&identity, "wrong-agent-id", "forged cancellation")
            .await
            .expect("wrong-owner failure attempt"),
        "a different edge owner must not terminate the dispatch"
    );
    assert_eq!(
        svc.admit_dispatch(&identity, &agent_id, r#"{"test":true}"#)
            .await
            .expect("dispatch remains replayable"),
        EdgeDispatchAdmission::Pending,
        "the forged failure must leave the durable dispatch executable"
    );
    assert!(
        svc.fail_dispatch(&identity, &agent_id, "cancelled")
            .await
            .expect("owner failure"),
        "the owning edge may terminate its own dispatch"
    );
    let EdgeDispatchAdmission::Terminal(result_json) = svc
        .admit_dispatch(&identity, &agent_id, r#"{"test":true}"#)
        .await
        .expect("failed dispatch replay")
    else {
        panic!("owner failure must create terminal durable evidence");
    };
    assert!(
        result_json.contains("cancelled"),
        "terminal replay must preserve the failure reason"
    );
}

/// wait_result times out if request never completes.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_wait_result_timeout() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-to-usr-{}", unique_suffix());
    let agent_id = format!("ed-to-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_id, &request_id);

    svc.insert_dispatch(&identity, &agent_id, "{}")
        .await
        .expect("insert_dispatch");

    // 200ms timeout — should not be enough to deliver result
    let result = svc
        .wait_result(&identity, std::time::Duration::from_millis(200))
        .await
        .expect("wait_result");
    assert!(result.is_none(), "wait_result should time out (Some=None)");
}

/// poll_pending is scoped to (user_id, edge_agent_id) — different user sees nothing.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_poll_isolation() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_a = format!("ed-iso-usr-a-{}", unique_suffix());
    let user_b = format!("ed-iso-usr-b-{}", unique_suffix());
    let agent = format!("ed-iso-agent-{}", unique_suffix());

    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_a, &request_id);
    svc.insert_dispatch(&identity, &agent, "{}")
        .await
        .expect("insert for user A");

    let rows = svc
        .poll_pending(&user_b, &agent)
        .await
        .expect("poll for user B");
    assert!(rows.is_empty(), "user B should not see user A's dispatch");
}

/// request_id uniqueness is scoped by owner; another user may use the same request_id
/// without seeing or completing the owner's dispatch.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_request_id_is_owner_scoped() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_a = format!("ed-own-usr-a-{}", unique_suffix());
    let user_b = format!("ed-own-usr-b-{}", unique_suffix());
    let agent_a = format!("ed-own-agent-a-{}", unique_suffix());
    let agent_b = format!("ed-own-agent-b-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity_a = dispatch_identity(&user_a, &request_id);
    let identity_b = dispatch_identity(&user_b, &request_id);

    svc.insert_dispatch(&identity_a, &agent_a, r#"{"owner":"a"}"#)
        .await
        .expect("insert user A dispatch");
    svc.insert_dispatch(&identity_b, &agent_b, r#"{"owner":"b"}"#)
        .await
        .expect("insert user B dispatch with same request_id");
    let owner_scope = sqlx::query(
        "SELECT COUNT(*) AS row_count, COUNT(DISTINCT user_id) AS owner_count \
         FROM edge_pending_dispatch \
         WHERE request_id = ? AND user_id IN (?, ?)",
    )
    .bind(&request_id)
    .bind(&user_a)
    .bind(&user_b)
    .fetch_one(pool.get())
    .await
    .expect("load owner-scoped dispatch rows");
    assert_eq!(
        owner_scope.try_get::<i64, _>("row_count").unwrap(),
        2,
        "same request_id across owners must create two dispatch rows"
    );
    assert_eq!(
        owner_scope.try_get::<i64, _>("owner_count").unwrap(),
        2,
        "same request_id across owners must remain isolated by user_id"
    );

    let wrong_owner = svc
        .deliver_result(&identity_b, &agent_a, r#"{"output":"stolen"}"#)
        .await
        .expect("wrong owner deliver_result");
    assert!(
        !wrong_owner,
        "matching request_id and agent_id must not cross owner boundary"
    );

    let ok_a = svc
        .deliver_result(&identity_a, &agent_a, r#"{"output":"owner-a"}"#)
        .await
        .expect("owner A deliver_result");
    assert!(ok_a, "owner A should complete its own dispatch");
    let ok_b = svc
        .deliver_result(&identity_b, &agent_b, r#"{"output":"owner-b"}"#)
        .await
        .expect("owner B deliver_result");
    assert!(ok_b, "owner B should complete its own dispatch");

    let result_a = svc
        .wait_result(&identity_a, std::time::Duration::from_secs(1))
        .await
        .expect("wait owner A result")
        .expect("owner A result");
    let result_b = svc
        .wait_result(&identity_b, std::time::Duration::from_secs(1))
        .await
        .expect("wait owner B result")
        .expect("owner B result");
    assert!(result_a.contains("owner-a"));
    assert!(result_b.contains("owner-b"));
}

/// cleanup_stale removes completed dispatches older than N seconds.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_cleanup_stale_removes_completed() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-cln-usr-{}", unique_suffix());
    let agent_id = format!("ed-cln-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_id, &request_id);

    svc.insert_dispatch(&identity, &agent_id, "{}")
        .await
        .expect("insert_dispatch");

    // Complete it
    svc.deliver_result(&identity, &agent_id, r#"{"done":true}"#)
        .await
        .expect("deliver_result");

    // Make only this test row stale. Using cleanup_stale(0) races under
    // nextest/online parallelism because it can expire unrelated pending
    // dispatches created by sibling tests.
    sqlx::query(
        "UPDATE edge_pending_dispatch \
         SET completed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) \
         WHERE user_id = ? AND request_id = ?",
    )
    .bind(&user_id)
    .bind(&request_id)
    .execute(pool.get())
    .await
    .expect("backdate completed dispatch");

    // Cleanup rows older than one day. The backdated row is removed without
    // touching newly-created pending rows from concurrently-running tests.
    let removed = svc
        .cleanup_stale(std::time::Duration::from_secs(24 * 60 * 60))
        .await
        .expect("cleanup_stale");
    assert!(removed >= 1, "should have removed at least 1 completed row");

    // wait_result now returns None (row was deleted)
    let result = svc
        .wait_result(&identity, std::time::Duration::from_millis(100))
        .await
        .expect("wait_result after cleanup");
    assert!(result.is_none(), "request should be gone after cleanup");
}

// ═══════════════════════════════════════════════════════════════════════
// Sweeper leases
// ═══════════════════════════════════════════════════════════════════════

/// Create a lease row, then verify it exists.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn sweeper_lease_create_and_query() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let sweeper_name = format!("test-sweeper-{}", unique_suffix());
    let pod_id = format!("pod-{}", unique_suffix());

    // Insert a lease
    sqlx::query(
        "INSERT INTO sweeper_leases (sweeper_name, owner_pod_id, expires_at) \
         VALUES (?, ?, DATE_ADD(NOW(6), INTERVAL 60 SECOND))",
    )
    .bind(&sweeper_name)
    .bind(&pod_id)
    .execute(pool.get())
    .await
    .expect("insert lease");

    // Verify it exists
    let row = sqlx::query(
        "SELECT sweeper_name, owner_pod_id, expires_at \
         FROM sweeper_leases WHERE sweeper_name = ?",
    )
    .bind(&sweeper_name)
    .fetch_one(pool.get())
    .await
    .expect("fetch lease");
    assert_eq!(row.get::<String, _>("sweeper_name"), sweeper_name);
    assert_eq!(row.get::<String, _>("owner_pod_id"), pod_id);
}

/// Acquire lease: INSERT ... ON DUPLICATE KEY ... WHERE expired.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn sweeper_lease_acquire_when_expired() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let sweeper_name = format!("acq-sweeper-{}", unique_suffix());
    let pod_a = format!("pod-a-{}", unique_suffix());
    let pod_b = format!("pod-b-{}", unique_suffix());

    // Insert an expired lease (past)
    sqlx::query(
        "INSERT INTO sweeper_leases (sweeper_name, owner_pod_id, expires_at) \
         VALUES (?, ?, DATE_SUB(NOW(6), INTERVAL 10 SECOND))",
    )
    .bind(&sweeper_name)
    .bind(&pod_a)
    .execute(pool.get())
    .await
    .expect("insert expired lease");

    // Acquire: update expired lease to new pod (any pod can acquire expired leases)
    let result = sqlx::query(
        "UPDATE sweeper_leases \
         SET owner_pod_id = ?, expires_at = DATE_ADD(NOW(6), INTERVAL 120 SECOND) \
         WHERE sweeper_name = ? AND expires_at < NOW(6)",
    )
    .bind(&pod_b)
    .bind(&sweeper_name)
    .execute(pool.get())
    .await
    .expect("acquire expired lease");
    assert_eq!(result.rows_affected(), 1, "should acquire expired lease");

    // Verify the lease is still owned by pod_b
    let row = sqlx::query(
        "SELECT owner_pod_id, expires_at \
         FROM sweeper_leases WHERE sweeper_name = ?",
    )
    .bind(&sweeper_name)
    .fetch_one(pool.get())
    .await
    .expect("fetch after renew");
    assert_eq!(row.get::<String, _>("owner_pod_id"), pod_b);
}

// ── Edge Registry Tests ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_register_list_unregister() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id_header = format!("edge_{}", unique_suffix());

    // Register
    let record = svc
        .register_or_update(
            &user_id,
            &edge_agent_id,
            &edge_id_header,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register");
    assert_eq!(record.user_id, user_id);
    assert_eq!(record.edge_agent_id, edge_agent_id);
    assert_eq!(record.edge_id, edge_id_header);

    // List returns the agent
    let list = svc.list_by_user(&user_id).await.expect("list_by_user");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].edge_agent_id, edge_agent_id);

    let replacement_edge_id = format!("edge-replacement-{}", uuid::Uuid::new_v4());
    svc.register_or_update(
        &user_id,
        &edge_agent_id,
        &replacement_edge_id,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("replace registration generation");
    assert!(
        !svc.unregister_generation(&user_id, &edge_agent_id, &edge_id_header)
            .await
            .expect("stale unregister is a successful no-op"),
        "stale connection generation must not delete its replacement"
    );

    // The current generation can unregister itself.
    assert!(
        svc.unregister_generation(&user_id, &edge_agent_id, &replacement_edge_id)
            .await
            .expect("unregister current generation")
    );

    // List returns empty
    let list = svc.list_by_user(&user_id).await.expect("list_by_user");
    assert_eq!(list.len(), 0);

    let inactive: (String, i8, Option<String>) = sqlx::query_as(
        "SELECT edge_id, registration_state, registration_claim_id \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&pool)
    .await
    .expect("read inactive registry owner");
    assert_eq!(inactive, (replacement_edge_id.clone(), 0, None));
    assert!(
        svc.unregister_generation(&user_id, &edge_agent_id, &replacement_edge_id)
            .await
            .expect("repeat unregister current generation"),
        "the retained inactive owner row makes repeated cleanup authoritative"
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_first_registration_rollback_retains_an_inactive_owner() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id = format!("edge_{}", unique_suffix());

    let lease = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &edge_id,
            Some("private-host"),
            Some("/private/worktree"),
            Some(serde_json::json!({"private": "capability"})),
            Some("private-workspace"),
        )
        .await
        .expect("claim first registration");
    let pending = registry_privacy_state(&pool, &user_id, &edge_agent_id).await;
    assert_eq!(pending.0, edge_id.clone());
    assert_eq!(pending.1, 0);
    assert_eq!(pending.2.as_deref(), lease.claim_id.as_deref());
    assert_eq!(
        (pending.3, pending.4, pending.5, pending.6),
        (None, None, None, None),
        "an unpublished first registration must not persist private metadata"
    );
    assert!(svc.rollback_registration(&lease).await.unwrap());
    assert!(
        svc.rollback_registration(&lease).await.unwrap(),
        "the inactive owner is durable idempotence evidence"
    );

    let inactive = registry_privacy_state(&pool, &user_id, &edge_agent_id).await;
    assert_eq!(inactive, (edge_id.clone(), 0, None, None, None, None, None));
    assert!(svc.list_by_user(&user_id).await.unwrap().is_empty());

    let successor = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-successor",
            Some("successor-host"),
            Some("/successor/worktree"),
            Some(serde_json::json!({"generation": "successor"})),
            Some("successor-workspace"),
        )
        .await
        .expect("reuse inactive owner for a later registration");
    assert!(successor.previous.is_none());
    let pending_successor = registry_privacy_state(&pool, &user_id, &edge_agent_id).await;
    assert_eq!(pending_successor.0, edge_id);
    assert_eq!(pending_successor.1, 0);
    assert_eq!(
        pending_successor.2.as_deref(),
        successor.claim_id.as_deref()
    );
    assert_eq!(
        (
            pending_successor.3,
            pending_successor.4,
            pending_successor.5,
            pending_successor.6,
        ),
        (None, None, None, None),
        "reusing an inactive owner must not persist successor metadata before finalize"
    );
    assert!(svc.finalize_registration(&successor).await.unwrap());
    assert!(svc.release_registration(&successor).await.unwrap());
    let published = svc.list_by_user(&user_id).await.unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].edge_id, "edge-successor");
    assert_eq!(published[0].hostname.as_deref(), Some("successor-host"));
    assert_eq!(
        published[0].worktree_path.as_deref(),
        Some("/successor/worktree")
    );
    assert_eq!(
        published[0].workspace_id.as_deref(),
        Some("successor-workspace")
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_heartbeat_unregistered() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id_header = format!("edge_{}", unique_suffix());

    let result = svc
        .heartbeat(&user_id, &edge_agent_id, &edge_id_header, None)
        .await;
    assert!(matches!(
        result.unwrap_err(),
        astra_services::HeartbeatError::StorageFailure(_)
    ));
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_concurrent_register() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc1 = DatabaseEdgeRegistryService::new(pool.clone());
    let svc2 = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id1 = format!("edge_{}", unique_suffix());
    let edge_id2 = format!("edge_{}", unique_suffix());

    // Concurrent registration with same key
    let (r1, r2) = tokio::join!(
        svc1.register_or_update(&user_id, &edge_agent_id, &edge_id1, None, None, None, None),
        svc2.register_or_update(&user_id, &edge_agent_id, &edge_id2, None, None, None, None)
    );

    let rec1 = r1.expect("register1");
    let rec2 = r2.expect("register2");

    // Both succeed, one is the final winner
    assert_eq!(rec1.user_id, user_id);
    assert_eq!(rec2.user_id, user_id);

    // List returns exactly one record
    let list = svc1.list_by_user(&user_id).await.expect("list");
    assert_eq!(list.len(), 1);
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_registration_lease_restores_the_exact_predecessor() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let old_edge_id = format!("edge_old_{}", unique_suffix());
    let new_edge_id = format!("edge_new_{}", unique_suffix());
    let old_capabilities = serde_json::json!({"generation": "old"});
    let new_capabilities = serde_json::json!({"generation": "new"});

    let predecessor = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &old_edge_id,
            Some("old-host"),
            Some("/old/worktree"),
            Some(old_capabilities.clone()),
            Some("workspace-old"),
        )
        .await
        .expect("register predecessor");
    assert!(
        svc.finalize_registration(&predecessor)
            .await
            .expect("finalize predecessor")
    );
    assert!(
        svc.release_registration(&predecessor)
            .await
            .expect("publish predecessor")
    );
    let replacement = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &new_edge_id,
            Some("new-host"),
            Some("/new/worktree"),
            Some(new_capabilities),
            Some("workspace-new"),
        )
        .await
        .expect("register replacement");

    assert_eq!(
        replacement
            .previous
            .as_ref()
            .map(|record| record.edge_id.as_str()),
        Some(old_edge_id.as_str())
    );
    assert!(
        svc.finalize_registration(&replacement)
            .await
            .expect("finalize replacement")
    );
    assert!(
        svc.rollback_registration(&replacement)
            .await
            .expect("rollback replacement")
    );
    assert!(
        svc.rollback_registration(&replacement)
            .await
            .expect("repeat rollback replacement"),
        "an already restored predecessor is an idempotent rollback success"
    );

    let restored = svc
        .list_by_user(&user_id)
        .await
        .expect("list restored predecessor")
        .into_iter()
        .next()
        .expect("restored predecessor record");
    assert_eq!(restored.edge_id, old_edge_id);
    assert_eq!(restored.hostname.as_deref(), Some("old-host"));
    assert_eq!(restored.worktree_path.as_deref(), Some("/old/worktree"));
    assert_eq!(restored.capabilities, Some(old_capabilities));
    assert_eq!(restored.workspace_id.as_deref(), Some("workspace-old"));
    let persisted: (String, i8, Option<String>) = sqlx::query_as(
        "SELECT edge_id, registration_state, registration_claim_id \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&pool)
    .await
    .expect("read persisted rollback state");
    assert_eq!(persisted, (old_edge_id, 1, None));
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_two_phase_registration_keeps_pending_metadata_unroutable() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    let published = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-old",
            Some("old-host"),
            Some("/workspace-old"),
            Some(serde_json::json!({"generation": "old"})),
            Some("workspace-old"),
        )
        .await
        .expect("claim old generation");
    assert!(svc.finalize_registration(&published).await.unwrap());
    assert!(svc.release_registration(&published).await.unwrap());

    let pending = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-new",
            Some("new-host"),
            Some("/workspace-new"),
            Some(serde_json::json!({"generation": "new"})),
            Some("workspace-new"),
        )
        .await
        .expect("claim new generation");
    let still_published = svc.list_by_user(&user_id).await.unwrap();
    assert_eq!(still_published.len(), 1);
    assert_eq!(still_published[0].edge_id, "edge-old");
    assert_eq!(
        still_published[0].workspace_id.as_deref(),
        Some("workspace-old")
    );
    svc.heartbeat(&user_id, &edge_agent_id, "edge-old", None)
        .await
        .expect("published predecessor remains healthy during claim");

    assert!(svc.finalize_registration(&pending).await.unwrap());
    assert!(
        svc.finalize_registration(&pending).await.unwrap(),
        "finalization must be idempotent while the same claim is retained"
    );
    assert!(
        svc.list_by_user(&user_id).await.unwrap().is_empty(),
        "finalized generation stays unroutable until pool commit releases the claim"
    );
    assert!(svc.release_registration(&pending).await.unwrap());
    assert!(
        svc.release_registration(&pending).await.unwrap(),
        "an already committed claim is an idempotent release success"
    );
    let current = svc.list_by_user(&user_id).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].edge_id, "edge-new");
    assert_eq!(current[0].workspace_id.as_deref(), Some("workspace-new"));
    let persisted: (String, i8, Option<String>) = sqlx::query_as(
        "SELECT edge_id, registration_state, registration_claim_id \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&pool)
    .await
    .expect("read persisted release state");
    assert_eq!(persisted, ("edge-new".to_string(), 1, None));
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_predecessor_disconnect_before_finalize_preserves_successor_claim() {
    require_env();
    let predecessor_pool = common::setup_pool().await.get().clone();
    let successor_pool = common::setup_pool().await.get().clone();
    let predecessor_pod = DatabaseEdgeRegistryService::new(predecessor_pool.clone());
    let successor_pod = DatabaseEdgeRegistryService::new(successor_pool);
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    publish_registry_predecessor(&predecessor_pod, &user_id, &edge_agent_id).await;
    let successor = claim_registry_successor(&successor_pod, &user_id, &edge_agent_id).await;
    assert!(
        predecessor_pod
            .unregister_generation(&user_id, &edge_agent_id, "edge-old")
            .await
            .expect("deactivate predecessor without erasing successor claim")
    );

    let pending = registry_privacy_state(&predecessor_pool, &user_id, &edge_agent_id).await;
    assert_eq!(pending.0, "edge-old");
    assert_eq!(pending.1, 0);
    assert_eq!(pending.2.as_deref(), successor.claim_id.as_deref());
    assert_eq!(
        (pending.3, pending.4, pending.5, pending.6),
        (None, None, None, None)
    );

    assert!(
        successor_pod
            .finalize_registration(&successor)
            .await
            .expect("finalize successor after predecessor disconnect")
    );
    assert!(
        successor_pod
            .release_registration(&successor)
            .await
            .expect("publish successor after predecessor disconnect")
    );
    let published = predecessor_pod.list_by_user(&user_id).await.unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].edge_id, "edge-new");
    assert_eq!(published[0].hostname.as_deref(), Some("new-host"));
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_predecessor_disconnect_after_finalize_prevents_rollback_resurrection() {
    require_env();
    let predecessor_pool = common::setup_pool().await.get().clone();
    let successor_pool = common::setup_pool().await.get().clone();
    let predecessor_pod = DatabaseEdgeRegistryService::new(predecessor_pool.clone());
    let successor_pod = DatabaseEdgeRegistryService::new(successor_pool);
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    publish_registry_predecessor(&predecessor_pod, &user_id, &edge_agent_id).await;
    let successor = claim_registry_successor(&successor_pod, &user_id, &edge_agent_id).await;
    assert!(
        successor_pod
            .finalize_registration(&successor)
            .await
            .expect("finalize successor")
    );
    assert!(
        predecessor_pod
            .unregister_generation(&user_id, &edge_agent_id, "edge-old")
            .await
            .expect("record predecessor disconnect after finalize")
    );

    let finalized: (String, i8, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT edge_id, registration_state, registration_claim_id, \
                    registration_previous_edge_id, hostname \
             FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&predecessor_pool)
    .await
    .expect("read finalized successor after predecessor disconnect");
    assert_eq!(finalized.0, "edge-new");
    assert_eq!(finalized.1, 2);
    assert_eq!(finalized.2.as_deref(), successor.claim_id.as_deref());
    assert_eq!(finalized.3, None);
    assert_eq!(finalized.4.as_deref(), Some("new-host"));

    assert!(
        successor_pod
            .rollback_registration(&successor)
            .await
            .expect("rollback successor without resurrecting disconnected predecessor")
    );
    assert!(
        successor_pod
            .rollback_registration(&successor)
            .await
            .expect("repeat rollback remains idempotent")
    );
    assert!(
        predecessor_pod
            .list_by_user(&user_id)
            .await
            .unwrap()
            .is_empty()
    );

    let inactive = registry_privacy_state(&predecessor_pool, &user_id, &edge_agent_id).await;
    assert_eq!(
        inactive,
        ("edge-new".to_string(), 0, None, None, None, None, None)
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_agent_lookup_isolated_by_workspace() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool);
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    let lease = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-workspace-owner",
            None,
            Some("/workspace/a"),
            None,
            Some("workspace-a"),
        )
        .await
        .expect("claim workspace-bound edge");
    assert!(svc.finalize_registration(&lease).await.unwrap());
    assert!(svc.release_registration(&lease).await.unwrap());

    assert!(
        svc.find_by_agent_id_and_workspace(&edge_agent_id, Some("workspace-a"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        svc.find_by_agent_id_and_workspace(&edge_agent_id, Some("workspace-b"))
            .await
            .unwrap()
            .is_none(),
        "a foreign workspace must not resolve the edge"
    );
    assert!(
        svc.find_by_agent_id_and_workspace(&edge_agent_id, None)
            .await
            .unwrap()
            .is_none(),
        "missing workspace context must not resolve a workspace-bound edge"
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_stale_lease_rollback_does_not_clobber_a_newer_generation() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool);

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let first_edge_id = format!("edge_first_{}", unique_suffix());
    let second_edge_id = format!("edge_second_{}", unique_suffix());
    let third_edge_id = format!("edge_third_{}", unique_suffix());

    let first = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &first_edge_id,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register first generation");
    assert!(
        svc.finalize_registration(&first)
            .await
            .expect("finalize first generation")
    );
    assert!(
        svc.release_registration(&first)
            .await
            .expect("publish first generation")
    );
    let stale_lease = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &second_edge_id,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register second generation");
    assert!(
        svc.finalize_registration(&stale_lease)
            .await
            .expect("finalize second generation")
    );
    assert!(
        svc.release_registration(&stale_lease)
            .await
            .expect("publish second generation")
    );
    let third = svc
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            &third_edge_id,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register third generation");
    assert!(
        svc.finalize_registration(&third)
            .await
            .expect("finalize third generation")
    );
    assert!(
        svc.release_registration(&third)
            .await
            .expect("publish third generation")
    );

    assert!(
        !svc.rollback_registration(&stale_lease)
            .await
            .expect("stale rollback is a successful no-op"),
        "a rollback must only restore the generation it replaced"
    );
    let current = svc
        .list_by_user(&user_id)
        .await
        .expect("list current generation")
        .into_iter()
        .next()
        .expect("current generation record");
    assert_eq!(current.edge_id, third_edge_id);
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_registration_claim_serializes_cross_pod_setup() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let first_pod = DatabaseEdgeRegistryService::new(pool.clone());
    let second_pod = DatabaseEdgeRegistryService::new(pool);
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    let held = first_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-first",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("first pod claims setup");
    let blocked = second_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-second",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(
        blocked.is_err(),
        "another pod must not build a rollback chain through an unpublished generation"
    );

    assert!(
        first_pod
            .finalize_registration(&held)
            .await
            .expect("finalize first pod claim")
    );
    assert!(
        first_pod
            .release_registration(&held)
            .await
            .expect("release first pod claim")
    );
    let successor = second_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-second",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("second pod claims after release");
    assert_eq!(
        successor
            .previous
            .as_ref()
            .map(|record| record.edge_id.as_str()),
        Some("edge-first")
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_finalized_claim_is_renewed_and_fences_a_third_generation() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let first_pod = DatabaseEdgeRegistryService::new(pool.clone());
    let second_pod = DatabaseEdgeRegistryService::new(pool.clone());
    let third_pod = DatabaseEdgeRegistryService::new(pool.clone());
    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());

    let first = first_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-first",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("claim first generation");
    assert!(first_pod.finalize_registration(&first).await.unwrap());
    assert!(first_pod.release_registration(&first).await.unwrap());

    let second = second_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-second",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("claim second generation");
    assert!(second_pod.finalize_registration(&second).await.unwrap());
    let second_claim = second.claim_id.as_deref().expect("durable second claim");

    sqlx::query(
        "UPDATE edge_agent_registry \
         SET registration_claim_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) \
         WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .execute(&pool)
    .await
    .expect("expire second claim for renewal test");
    second_pod
        .heartbeat(&user_id, &edge_agent_id, "edge-second", Some(second_claim))
        .await
        .expect("the finalized owner renews its exact claim");

    let renewed: (Option<String>, i8) = sqlx::query_as(
        "SELECT registration_claim_id, registration_claim_expires_at > NOW(6) \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&pool)
    .await
    .expect("read renewed claim");
    assert_eq!(renewed, (Some(second_claim.to_string()), 1));

    let blocked_third = third_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-third-blocked",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(
        blocked_third.is_err(),
        "a healthy finalized owner must not lose its renewed claim"
    );

    sqlx::query(
        "UPDATE edge_agent_registry \
         SET registration_claim_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) \
         WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .execute(&pool)
    .await
    .expect("expire abandoned second claim");
    let third = third_pod
        .register_or_update_with_lease(
            &user_id,
            &edge_agent_id,
            "edge-third",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("third generation takes an expired finalized claim");

    assert!(matches!(
        second_pod
            .heartbeat(&user_id, &edge_agent_id, "edge-second", Some(second_claim),)
            .await,
        Err(astra_services::multi_agent::HeartbeatError::Superseded)
    ));
    assert!(third_pod.rollback_registration(&third).await.unwrap());
    assert!(third_pod.list_by_user(&user_id).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_register_twice_updates() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id1 = format!("edge_{}", unique_suffix());
    let edge_id2 = format!("edge_{}", unique_suffix());
    let hostname1 = Some("host1");
    let hostname2 = Some("host2");

    // First register
    let rec1 = svc
        .register_or_update(
            &user_id,
            &edge_agent_id,
            &edge_id1,
            hostname1,
            None,
            None,
            None,
        )
        .await
        .expect("register1");
    assert_eq!(rec1.edge_id, edge_id1);
    assert_eq!(rec1.hostname, Some("host1".to_string()));

    // Second register updates
    let rec2 = svc
        .register_or_update(
            &user_id,
            &edge_agent_id,
            &edge_id2,
            hostname2,
            None,
            None,
            None,
        )
        .await
        .expect("register2");
    assert_eq!(rec2.edge_id, edge_id2);
    assert_eq!(rec2.hostname, Some("host2".to_string()));

    // List returns one record with updated fields
    let list = svc.list_by_user(&user_id).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].edge_id, edge_id2);
    assert_eq!(list[0].hostname, Some("host2".to_string()));
}

// ── Concurrent poll_pending tests ────────────────────────────────────

/// Two concurrent poll_pending calls must NOT claim the same row.
/// The first transaction commits (SET dispatched), the second sees
/// status='dispatched' and skips it. Assert exactly one winner
/// and the dispatch status is correct.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_concurrent_poll_single_winner() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let user_id = format!("ed-cc-usr-{}", unique_suffix());
    let agent_id = format!("ed-cc-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();
    let identity = dispatch_identity(&user_id, &request_id);

    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    svc.insert_dispatch(&identity, &agent_id, r#"{"tool":"bash"}"#)
        .await
        .expect("insert_dispatch");

    use tokio::sync::Barrier;
    let barrier = std::sync::Arc::new(Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let pool1 = pool.get().clone();
    let pool2 = pool.get().clone();
    let user1 = user_id.clone();
    let user2 = user_id.clone();
    let ag1 = agent_id.clone();
    let ag2 = agent_id.clone();

    let (r1, r2) = tokio::join!(
        async move {
            b1.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool1);
            svc.poll_pending(&user1, &ag1).await
        },
        async move {
            b2.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool2);
            svc.poll_pending(&user2, &ag2).await
        }
    );

    let rows1 = r1.expect("poll 1");
    let rows2 = r2.expect("poll 2");

    let total = rows1.len() + rows2.len();
    assert_eq!(total, 1, "exactly one poll call should claim the row");
    assert!(
        rows1.is_empty() ^ rows2.is_empty(),
        "one poll should be empty, the other gets the row"
    );

    // Verify the claimed row has status='dispatched'
    let claimed_rows = if !rows1.is_empty() { &rows1 } else { &rows2 };
    assert_eq!(claimed_rows[0].request_id, request_id);
    assert_eq!(
        claimed_rows[0].status, "dispatched",
        "claimed row status must be 'dispatched'"
    );

    // poll_pending again — must be empty
    let rows3 = svc.poll_pending(&user_id, &agent_id).await.expect("poll 3");
    assert!(rows3.is_empty(), "no more pending after concurrent claim");
}

/// With N pending rows and M concurrent pollers, verify exactly N rows
/// are claimed total and status='dispatched' on each.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_concurrent_poll_multi_row_multi_poller() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let user_id = format!("ed-cm-usr-{}", unique_suffix());
    let agent_id = format!("ed-cm-agent-{}", unique_suffix());

    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    // Insert 3 pending rows
    let mut request_ids = Vec::new();
    for i in 0..3 {
        let rid = format!("{}-{}", Uuid::new_v4(), i);
        let identity = dispatch_identity(&user_id, &rid);
        svc.insert_dispatch(&identity, &agent_id, r#"{"test":true}"#)
            .await
            .expect("insert_dispatch");
        request_ids.push(rid);
    }

    // 4 concurrent pollers for 3 rows — one poller must get nothing
    use tokio::sync::Barrier;
    let n = 4;
    let barrier = std::sync::Arc::new(Barrier::new(n));
    let mut handles = Vec::new();

    for _ in 0..n {
        let pool = pool.get().clone();
        let user = user_id.clone();
        let agent = agent_id.clone();
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool);
            svc.poll_pending(&user, &agent).await
        }));
    }

    let mut total_claimed = 0usize;
    for h in handles {
        let rows = h.await.expect("join").expect("poll");
        for row in &rows {
            assert_eq!(
                row.status, "dispatched",
                "every claimed row must have status='dispatched'"
            );
        }
        total_claimed += rows.len();
    }

    assert_eq!(total_claimed, 3, "exactly 3 rows should be claimed total");
    // No more pending
    let remaining = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("final poll");
    assert!(remaining.is_empty(), "no rows should remain after claim");
}
