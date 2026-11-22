//! MatrixOne-backed integration tests for edge dispatch and sweeper leases.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test edge_dispatch_db_it -- --ignored --test-threads=1

mod common;

use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use common::mysql_response_proxy::{Boundary, ResponseProxy, proxy_pool, proxy_pool_with_limit};

use astra_services::multi_agent::{
    DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, EdgeDirectDispatchAdmission,
    EdgeDispatchAdmission, EdgeDispatchAdmissionError, EdgeDispatchDelivery,
    EdgeDispatchDeliveryError, EdgeDispatchFailure, EdgeDispatchFailureCause,
    EdgeDispatchFailureStage, EdgeDispatchIdentity, EdgeDispatchService, EdgeRegistrationLease,
    EdgeRegistryService,
};
use sqlx::Row;
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────

fn parse_dispatch_payload(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("valid dispatch fixture")
}

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
    svc.admit_dispatch(&identity, &agent_id, &parse_dispatch_payload(payload))
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
        .wait_result(&identity, Some(std::time::Duration::from_secs(5)))
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
async fn edge_dispatch_concurrent_conflicting_results_have_one_durable_winner() {
    require_env();
    let pool = common::setup_pool().await;
    let first = DatabaseEdgeDispatchService::new(pool.get().clone());
    let second = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("ed-conflict-{}", unique_suffix());
    let agent = format!("ed-agent-{}", unique_suffix());
    let identity = dispatch_identity(&user, &Uuid::new_v4().to_string());
    first
        .admit_dispatch(&identity, &agent, &serde_json::json!({"tool": "read_file"}))
        .await
        .unwrap();
    let a = r#"{"output":"first"}"#;
    let b = r#"{"output":"second"}"#;
    let (a_result, b_result) = tokio::join!(
        first.deliver_result(&identity, &agent, a),
        second.deliver_result(&identity, &agent, b),
    );
    assert!(
        !(matches!(a_result, Ok(true)) && matches!(b_result, Ok(true))),
        "different outcomes must never both be accepted"
    );
    let stored = first
        .wait_result(&identity, Some(std::time::Duration::ZERO))
        .await
        .unwrap()
        .expect("one outcome is durable");
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    let (winner, loser) = if stored["output"] == "first" {
        (a, b)
    } else {
        assert_eq!(stored["output"], "second");
        (b, a)
    };
    assert!(
        second
            .deliver_result(&identity, &agent, winner)
            .await
            .unwrap()
    );
    assert!(matches!(
        second.deliver_result(&identity, &agent, loser).await,
        Err(EdgeDispatchDeliveryError::PayloadConflict)
    ));
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_large_result_survives_cross_instance_recovery() {
    require_env();
    let pool = common::setup_pool().await;
    let producer = DatabaseEdgeDispatchService::new(pool.get().clone());
    let reader = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("ed-large-{}", unique_suffix());
    let agent = format!("ed-agent-{}", unique_suffix());
    let identity = dispatch_identity(&user, &Uuid::new_v4().to_string());
    let payload = r#"{"tool":"bash","args":{"command":"cat log"}}"#;
    let result =
        serde_json::json!({"status":"completed", "output":"日志\\n".repeat(20_000)}).to_string();
    assert!(result.len() > 100_000);
    producer
        .admit_dispatch(&identity, &agent, &parse_dispatch_payload(payload))
        .await
        .unwrap();
    assert!(
        producer
            .deliver_result(&identity, &agent, &result)
            .await
            .unwrap()
    );

    let exact = reader
        .wait_result(&identity, Some(std::time::Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(exact.as_deref(), Some(result.as_str()));
    let batched = reader
        .wait_result(&identity, Some(std::time::Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(batched.as_deref(), Some(result.as_str()));
    assert_eq!(
        reader
            .admit_dispatch(&identity, &agent, &parse_dispatch_payload(payload))
            .await
            .unwrap(),
        EdgeDispatchAdmission::Terminal(result.clone()),
    );
    assert!(
        reader
            .deliver_result(&identity, &agent, &result)
            .await
            .unwrap()
    );
    let other = dispatch_identity(&format!("other-{user}"), &identity.request_id);
    assert!(
        reader
            .wait_result(&other, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
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
        svc.admit_dispatch(&identity, &agent_id, &parse_dispatch_payload(payload))
            .await
            .expect("first admission"),
        EdgeDispatchAdmission::Pending
    );
    assert!(matches!(
        svc.admit_dispatch(&identity, "different-agent", &parse_dispatch_payload(payload))
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(message)) if message.contains("conflicts")
    ));
    assert!(matches!(
        svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"tool":"different"}))
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(message)) if message.contains("conflicts")
    ));
    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&identity, &agent_id, &parse_dispatch_payload(payload))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Claimed,
    );
    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&identity, &agent_id, &parse_dispatch_payload(payload))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Observing,
        "only one delivery path may own the durable dispatch boundary"
    );

    let result_json = r#"{"status":"completed","output":"once"}"#;
    assert!(
        svc.deliver_result(&identity, &agent_id, result_json)
            .await
            .expect("terminal result")
    );
    let EdgeDispatchAdmission::Terminal(replayed_json) = svc
        .admit_dispatch(&identity, &agent_id, &parse_dispatch_payload(payload))
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
async fn edge_dispatch_admission_reports_database_access_denied_without_secrets() {
    require_env();
    let (_pool, settings) = common::setup_pool_and_settings().await;
    let options = sqlx::mysql::MySqlConnectOptions::new()
        .host(&settings.host)
        .port(settings.port)
        .username(&settings.user)
        .password("fixture-invalid-password")
        .database(&settings.database);
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect_lazy_with(options);
    let service = DatabaseEdgeDispatchService::new(pool.clone());
    let identity = dispatch_identity("auth-test-user", "auth-test-request");
    let error = service
        .admit_dispatch(&identity, "edge", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        EdgeDispatchAdmissionError::OutcomeUnknown(EdgeDispatchFailure {
            stage: EdgeDispatchFailureStage::Begin,
            cause: EdgeDispatchFailureCause::AccessDenied,
        })
    );
    assert!(!error.to_string().contains("fixture-invalid-password"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_concurrent_direct_admission_has_one_owner() {
    require_env();
    let pool = common::setup_pool().await;
    let service = Arc::new(DatabaseEdgeDispatchService::new(pool.get().clone()));
    let identity = dispatch_identity(&format!("ed-race-{}", unique_suffix()), "request");
    let payload = Arc::new(parse_dispatch_payload(
        r#"{"tool":"bash","args":{"fraction":2.291712365432881e-9}}"#,
    ));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (service, identity, payload, barrier) = (
            service.clone(),
            identity.clone(),
            payload.clone(),
            barrier.clone(),
        );
        tasks.spawn(async move {
            barrier.wait().await;
            service
                .admit_and_claim_direct_dispatch(&identity, "edge", &payload)
                .await
        });
    }
    let (mut claimed, mut observing) = (0, 0);
    while let Some(result) = tasks.join_next().await {
        match result.unwrap().unwrap() {
            EdgeDirectDispatchAdmission::Claimed => claimed += 1,
            EdgeDirectDispatchAdmission::Observing => observing += 1,
            other => panic!("unexpected admission: {other:?}"),
        }
    }
    assert_eq!((claimed, observing), (1, 7));
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&identity.user_id)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_preserves_numeric_identity_across_database_roundtrips() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("ed-numeric-{}", unique_suffix());
    let edge = format!("edge-numeric-{}", unique_suffix());
    let direct = dispatch_identity(&user, &unique_suffix());
    let relay = dispatch_identity(&user, &unique_suffix());
    let payload = parse_dispatch_payload(
        r#"{"tool":"bash","args":{"command":"git status","timeout":120.0,"fraction":2.291712365432881e-9,"nested":[-120.0,-0.0,0.125,1e2,1e20,8.949943645992772e17],"large":9007199254740993,"max":18446744073709551615}}"#,
    );
    let reordered = parse_dispatch_payload(
        r#"{"args":{"max":18446744073709551615,"large":9007199254740993,"nested":[-120.0,-0.0,0.125,1e2,1e20,8.949943645992772e17],"fraction":2.291712365432881e-9,"timeout":120.0,"command":"git status"},"tool":"bash"}"#,
    );
    let replay = parse_dispatch_payload(&payload.to_string());
    assert_eq!(
        replay, payload,
        "persisted JSON must replay the same numbers"
    );
    let fraction = payload["args"]["fraction"].as_f64().unwrap();
    assert_eq!(
        replay["args"]["fraction"].as_f64().unwrap().to_bits(),
        fraction.to_bits()
    );
    let mut different_large = payload.clone();
    different_large["args"]["large"] = serde_json::json!(9007199254740992_u64);
    let mut different_type = payload.clone();
    different_type["args"]["timeout"] = serde_json::json!(120);
    let mut different_zero = payload.clone();
    different_zero["args"]["nested"][1] = serde_json::json!(0.0);
    let mut different_fraction = payload.clone();
    different_fraction["args"]["fraction"] =
        serde_json::json!(f64::from_bits(fraction.to_bits() + 1));

    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&direct, &edge, &payload)
            .await
            .expect("first direct admission"),
        EdgeDirectDispatchAdmission::Claimed
    );
    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&direct, &edge, &replay)
            .await
            .expect("serialized direct replay"),
        EdgeDirectDispatchAdmission::Observing
    );
    for changed in [
        &different_large,
        &different_type,
        &different_zero,
        &different_fraction,
    ] {
        assert!(matches!(
            svc.admit_and_claim_direct_dispatch(&direct, &edge, changed)
                .await,
            Err(EdgeDispatchAdmissionError::Rejected(_))
        ));
    }
    assert!(matches!(
        svc.admit_and_claim_direct_dispatch(&direct, "different-edge", &payload)
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(_))
    ));

    assert_eq!(
        svc.admit_dispatch(&relay, &edge, &payload).await.unwrap(),
        EdgeDispatchAdmission::Pending
    );
    assert_eq!(
        svc.admit_dispatch(&relay, &edge, &replay).await.unwrap(),
        EdgeDispatchAdmission::Pending
    );
    let rows = svc.poll_pending(&user, &edge).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_id, relay.request_id);
    assert_eq!(
        rows[0].payload_json,
        payload.to_string(),
        "relay must retain the exact serialized request"
    );

    let result = r#"{"status":"completed","output":"ok"}"#;
    for identity in [&direct, &relay] {
        assert!(svc.deliver_result(identity, &edge, result).await.unwrap());
        assert_eq!(
            svc.admit_dispatch(identity, &edge, &reordered)
                .await
                .unwrap(),
            EdgeDispatchAdmission::Terminal(result.to_owned())
        );
    }
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn rejected_relay_admission_never_publishes_the_candidate() {
    require_env();
    let pool = common::setup_pool().await;
    let service = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("ed-rejected-{}", unique_suffix());
    let identity = dispatch_identity(&user, &unique_suffix());
    // An over-width owner must be rejected before a relay can observe a row.
    let oversized_edge = "e".repeat(256);
    let stored_edge = &oversized_edge[..255];
    let payload = serde_json::json!({"tool":"bash","args":{"command":"printf once"}});
    let barrier = tokio::sync::Barrier::new(2);
    let (admission, observed) = tokio::join!(
        async {
            barrier.wait().await;
            service
                .admit_dispatch(&identity, &oversized_edge, &payload)
                .await
        },
        async {
            barrier.wait().await;
            let mut observed = Vec::new();
            for _ in 0..8 {
                observed.extend(service.poll_pending(&user, stored_edge).await.unwrap());
                tokio::task::yield_now().await;
            }
            observed
        }
    );
    assert!(matches!(
        admission,
        Err(EdgeDispatchAdmissionError::Rejected(_))
    ));
    assert!(observed.is_empty(), "a rejected request became claimable");
    assert!(
        service
            .poll_pending(&user, stored_edge)
            .await
            .unwrap()
            .is_empty()
    );
    let oversized_payload = serde_json::json!({
        "tool": "write_file", "args": {"content": "x".repeat(4 * 1024 * 1024)}
    });
    assert!(matches!(
        service
            .admit_dispatch(&identity, stored_edge, &oversized_payload)
            .await,
        Err(EdgeDispatchAdmissionError::Rejected(_))
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM edge_pending_dispatch WHERE user_id = ?")
            .bind(&user)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(count, 0, "rejection must leave no inserted row");
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn uncommitted_relay_candidate_is_not_visible_to_poll() {
    require_env();
    let pool = common::setup_pool().await;
    let user = format!("ed-uncommitted-{}", unique_suffix());
    let edge = format!("edge-uncommitted-{}", unique_suffix());
    let identity = dispatch_identity(&user, &unique_suffix());
    let mut tx = pool.get().begin().await.unwrap();
    sqlx::query(
        "INSERT INTO edge_pending_dispatch \
         (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, run_owner_user_id, payload_digest, payload_json, status) \
         VALUES (?, ?, ?, ?, ?, ?, ?, '', ?, 'pending')",
    )
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .bind(&identity.run_id)
    .bind(&identity.turn_chain_id)
    .bind(&edge)
    .bind(&identity.request_id)
    .bind(&identity.user_id)
    .bind(r#"{"tool":"bash"}"#)
    .execute(&mut *tx)
    .await
    .unwrap();

    let ready = Arc::new(tokio::sync::Barrier::new(2));
    let mut poll = {
        let service = DatabaseEdgeDispatchService::new(pool.get().clone());
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.wait().await;
            service.poll_pending(&user, &edge).await
        })
    };
    ready.wait().await;
    let before_rollback =
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut poll).await;
    tx.rollback().await.unwrap();
    let rows = match before_rollback {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => tokio::time::timeout(std::time::Duration::from_secs(3), poll)
            .await
            .expect("poll must finish after rollback")
            .unwrap()
            .unwrap(),
    };
    assert!(
        rows.is_empty(),
        "an uncommitted candidate must never be claimed"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_reuses_a_connection_across_nullable_insert_shapes() {
    require_env();
    let (_, settings) = common::setup_pool_and_settings().await;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&settings.database_url_with_password())
        .await
        .unwrap();
    let service = DatabaseEdgeDispatchService::new(pool.clone());
    let user = format!("ed-shapes-{}", unique_suffix());
    let direct = dispatch_identity(&user, &unique_suffix());
    let relay = dispatch_identity(&user, &unique_suffix());
    assert_eq!(
        service
            .admit_and_claim_direct_dispatch(&direct, "edge", &serde_json::json!({}))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Claimed
    );
    let payload = serde_json::json!({"tool": "bash", "args":{"command": "printf ready"}});
    assert_eq!(
        service
            .admit_dispatch(&relay, "edge", &payload)
            .await
            .unwrap(),
        EdgeDispatchAdmission::Pending
    );
    let claimed = service.poll_pending(&user, "edge").await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].identity(), relay);
    assert_eq!(parse_dispatch_payload(&claimed[0].payload_json), payload);
    let observed = service
        .inspect_run_dispatches(&user, &relay.session_id, &relay.run_id, None, None)
        .await
        .unwrap();
    assert_eq!(observed[0].tool_name.as_deref(), Some("bash"));
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&user)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_large_request_preserves_admission_and_replay() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("ed-large-{}", unique_suffix());
    let edge = format!("edge-large-{}", unique_suffix());
    let identity = dispatch_identity(&user, &unique_suffix());
    let payload =
        serde_json::json!({"tool": "write_file", "args": {"content": "x".repeat(300 * 1024)}})
            .to_string();

    assert_eq!(
        svc.admit_dispatch(&identity, &edge, &parse_dispatch_payload(&payload))
            .await
            .unwrap(),
        EdgeDispatchAdmission::Pending
    );
    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&identity, &edge, &parse_dispatch_payload(&payload))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Claimed
    );
    let body: Option<String> = sqlx::query_scalar(
        "SELECT payload_json FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?",
    )
    .bind(&user)
    .bind(&identity.request_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert!(
        body.is_none(),
        "a direct takeover must discard the queued request body"
    );

    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&identity, &edge, &parse_dispatch_payload(&payload))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Observing
    );
    let result = r#"{"status":"completed","output":"written"}"#;
    assert!(svc.deliver_result(&identity, &edge, result).await.unwrap());
    assert_eq!(
        svc.admit_dispatch(&identity, &edge, &parse_dispatch_payload(&payload))
            .await
            .unwrap(),
        EdgeDispatchAdmission::Terminal(result.to_string())
    );
    assert_eq!(
        svc.admit_and_claim_direct_dispatch(&identity, &edge, &parse_dispatch_payload(&payload))
            .await
            .unwrap(),
        EdgeDirectDispatchAdmission::Terminal(result.to_string())
    );
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
}

const BATCHED_RESULT_SELECT: &[u8] =
    b"SELECT user_id, session_id, run_id, turn_chain_id, request_id, status, result_json";
const OBSERVED_RECEIPT: &str = r#"{"status":"completed","output":"ready"}"#;

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_abandoned_read_does_not_block_unrelated_result_recovery() {
    abandoned_batch_yields(Boundary::Rows(BATCHED_RESULT_SELECT), 1).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_abandoned_progressing_batch_yields_to_new_waiters() {
    // Each row arrives soon enough to renew a progress timer, but the
    // complete result set takes longer than the new caller's budget.
    abandoned_batch_yields(
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(590)),
        8,
    )
    .await;
}

async fn abandoned_batch_yields(boundary: Boundary, old_count: usize) {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = format!("ed-wait-{}", unique_suffix());
    let older: Vec<_> = (0..old_count)
        .map(|index| dispatch_identity(&user, &format!("old-{index}")))
        .collect();
    let ready = dispatch_identity(&user, "ready");
    for identity in older.iter().chain(std::iter::once(&ready)) {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );

    let proxy = ResponseProxy::start(settings.host.clone(), settings.port, boundary).await;
    let observer_pool = proxy_pool(&settings, proxy.port).await;
    let observer = Arc::new(DatabaseEdgeDispatchService::from_shared(&observer_pool));
    let abandoned = tokio::spawn({
        let observer = observer.clone();
        async move {
            futures_util::future::join_all(
                older
                    .iter()
                    .map(|identity| observer.wait_result(identity, None)),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the old batch must reach MatrixOne before its callers cancel");
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());

    // The old response is never explicitly released. The other session
    // must recover its result from the same one-connection pool.
    assert_eq!(
        observer
            .wait_result(&ready, Some(Duration::from_secs(3)))
            .await
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT)
    );
    observer_pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_slow_result_uses_the_caller_budget_and_reuses_the_connection() {
    observe_terminal_row(
        Boundary::TailAfter(
            BATCHED_RESULT_SELECT,
            Duration::from_millis(1_500),
            Duration::from_millis(75),
        ),
        true,
        false,
        Duration::from_secs(3),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_healthy_batch_is_read_once() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    for cancel_peer in [false, true] {
        let user = unique_suffix();
        let ready = dispatch_identity(&user, "ready");
        let peer = dispatch_identity(&user, "peer");
        for identity in [&ready, &peer] {
            producer
                .admit_dispatch(identity, "edge", &serde_json::json!({}))
                .await
                .unwrap();
            assert!(
                producer
                    .deliver_result(identity, "edge", OBSERVED_RECEIPT)
                    .await
                    .unwrap()
            );
        }
        let proxy = ResponseProxy::start(
            settings.host.clone(),
            settings.port,
            Boundary::Rows(BATCHED_RESULT_SELECT),
        )
        .await;
        let observed = proxy_pool(&settings, proxy.port).await;
        let observer = DatabaseEdgeDispatchService::from_shared(&observed);
        let timeout = Duration::from_secs(3);
        let mut ready = Box::pin(observer.wait_result(&ready, Some(timeout)));
        let mut peer = Box::pin(observer.wait_result(&peer, Some(timeout)));
        assert!(futures_util::poll!(&mut ready).is_pending());
        assert!(futures_util::poll!(&mut peer).is_pending());
        tokio::time::timeout(Duration::from_secs(1), proxy.gate.received.notified())
            .await
            .expect("the batch reached the database");

        let release_at = tokio::time::Instant::now() + Duration::from_millis(2450);
        if cancel_peer {
            tokio::time::sleep(Duration::from_millis(900)).await;
            drop(peer);
            tokio::time::sleep_until(release_at).await;
            proxy.gate.release.notify_one();
        } else {
            // Both callbacks have only the final three-second recovery
            // window. The database response still fits in that window.
            tokio::time::sleep_until(release_at).await;
            proxy.gate.release.notify_one();
            assert_eq!(peer.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
        }
        assert_eq!(ready.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
        assert_eq!(proxy.gate.matched.load(Ordering::SeqCst), 1);
        observed.get().close().await;
        sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
            .bind(user)
            .execute(pool.get())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_publishes_terminal_row_before_the_end_of_the_response() {
    observe_terminal_row(
        Boundary::RowTerminator(BATCHED_RESULT_SELECT),
        false,
        false,
        Duration::from_secs(3),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_competing_waiter_does_not_interrupt_a_slow_healthy_turn() {
    observe_terminal_row(
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(900)),
        true,
        true,
        Duration::from_secs(3),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_competing_slow_reads_complete_without_an_unnecessary_yield() {
    observe_terminal_row(
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(1_500)),
        true,
        true,
        Duration::from_secs(6),
    )
    .await;
}

async fn observe_terminal_row(
    boundary: Boundary,
    expect_reused: bool,
    with_competitor: bool,
    caller_budget: Duration,
) {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let ready = dispatch_identity(&user, "ready");
    let competitor = with_competitor.then(|| dispatch_identity(&user, "competing"));
    for identity in std::iter::once(&ready).chain(competitor.as_ref()) {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            producer
                .deliver_result(identity, "edge", OBSERVED_RECEIPT)
                .await
                .unwrap()
        );
    }

    let proxy = ResponseProxy::start(settings.host.clone(), settings.port, boundary).await;
    proxy
        .gate
        .continuous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let observer_pool = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observer_pool);
    let connection: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(observer_pool.get())
        .await
        .unwrap();
    let (result, ()) = tokio::join!(observer.wait_result(&ready, Some(caller_budget)), async {
        tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
            .await
            .expect("the proxy must delay the selected response boundary");
        if let Some(competitor) = competitor {
            assert_eq!(
                observer
                    .wait_result(
                        &competitor,
                        Some(caller_budget - Duration::from_millis(250))
                    )
                    .await
                    .unwrap()
                    .as_deref(),
                Some(OBSERVED_RECEIPT),
            );
        }
    },);
    assert_eq!(result.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    let next_connection: u64 = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar("SELECT CONNECTION_ID()").fetch_one(observer_pool.get()),
    )
    .await
    .expect("a held terminator must release the pool slot after delivery")
    .unwrap();
    assert_eq!(connection == next_connection, expect_reused);
    observer_pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_earlier_deadline_keeps_a_slow_healthy_read() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let ready = dispatch_identity(&user, "slow-ready");
    let pending = dispatch_identity(&user, "later-deadline-pending");
    for identity in [&ready, &pending] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(2_450)),
    )
    .await;
    proxy
        .gate
        .continuous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);
    let mut wait = Box::pin(observer.wait_result(&ready, Some(Duration::from_secs(3))));
    assert!(futures_util::poll!(&mut wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the ready row is delayed by the proxy");
    let mut competitor = Box::pin(observer.wait_result(&pending, Some(Duration::from_secs(5))));
    assert!(futures_util::poll!(&mut competitor).is_pending());

    assert_eq!(wait.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    drop(competitor);
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_pending_waiter_stays_separate_from_stalled_keys() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let old = dispatch_identity(&user, "blocked-old-key");
    let ready = dispatch_identity(&user, "ready");
    for identity in [&old, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    // Every response containing the old key stalls, including retries. The
    // new key must stay separate even if its first read still sees Pending.
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"blocked-old-key"),
    )
    .await;
    proxy
        .gate
        .continuous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = Arc::new(DatabaseEdgeDispatchService::from_shared(&observed));
    let mut old_waiter = tokio::spawn({
        let observer = observer.clone();
        let old = old.clone();
        async move {
            observer
                .wait_result(&old, Some(Duration::from_secs(20)))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the old read has reached the proxy");
    // A second subscriber gives the stalled batch a comparable deadline.
    let mut old_short_wait = Box::pin(observer.wait_result(&old, Some(Duration::from_secs(3))));
    assert!(futures_util::poll!(&mut old_short_wait).is_pending());

    let late_receipt = tokio::time::Instant::now() + Duration::from_millis(2_650);
    let (result, ()) = tokio::join!(
        observer.wait_result(&ready, Some(Duration::from_secs(3))),
        async {
            tokio::time::sleep_until(late_receipt).await;
            assert!(
                producer
                    .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
                    .await
                    .unwrap()
            );
        },
    );
    assert_eq!(result.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    drop(old_short_wait);
    assert!(
        !old_waiter.is_finished(),
        "yielding the SQL must preserve its active waiter"
    );

    proxy
        .gate
        .armed
        .store(false, std::sync::atomic::Ordering::SeqCst);
    proxy
        .gate
        .continuous
        .store(false, std::sync::atomic::Ordering::SeqCst);
    proxy.gate.release.notify_one();
    assert!(
        producer
            .deliver_result(&old, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), &mut old_waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT),
    );
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_rechecks_pending_result_while_another_batch_progresses() {
    for wait_budget in [Duration::from_secs(3), Duration::from_secs(6)] {
        recheck_pending_result_while_another_batch_progresses(wait_budget).await;
    }
}

async fn recheck_pending_result_while_another_batch_progresses(wait_budget: Duration) {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let old: Vec<_> = (0..4)
        .map(|index| dispatch_identity(&user, &format!("progressing-{index}")))
        .collect();
    let ready = dispatch_identity(&user, "finishes-during-old-read");
    for identity in old.iter().chain([&ready]) {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    // Only the first query drips rows. Its subscribers stay alive while the
    // second batch first sees Pending and then rechecks on the spare slot.
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(1550)),
    )
    .await;
    let observed = proxy_pool_with_limit(&settings, proxy.port, 3).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);
    let mut earlier = Box::pin(futures_util::future::join_all(
        old.iter()
            .map(|identity| observer.wait_result(identity, None)),
    ));
    assert!(futures_util::poll!(&mut earlier).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the first batch has begun reading");

    let (fresh, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(3),
            observer.wait_result(&ready, Some(wait_budget)),
        ),
        async {
            let first_read_budget = Duration::from_secs(if wait_budget <= Duration::from_secs(3) {
                1
            } else {
                2
            });
            tokio::time::timeout(first_read_budget, async {
                while proxy.gate.matched.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the new batch has issued its first read");
            tokio::time::sleep(Duration::from_millis(350)).await;
            assert!(
                producer
                    .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
                    .await
                    .unwrap()
            );
        }
    );
    assert_eq!(fresh.unwrap().unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert!(proxy.gate.matched.load(Ordering::SeqCst) >= 3);
    drop(earlier);
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_yield_resnapshots_before_querying_more_old_batches() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let anchor = dispatch_identity(&user, "old-stall-anchor");
    let ready = dispatch_identity(&user, "ready");
    let backlog: Vec<_> = (0..384)
        .map(|index| dispatch_identity(&user, &format!("old-stall-{index}")))
        .collect();
    for identity in [&anchor, &ready].into_iter().chain(&backlog) {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );

    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"old-stall-"),
    )
    .await;
    proxy
        .gate
        .continuous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);
    let mut anchor_wait = Box::pin(observer.wait_result(&anchor, None));
    assert!(futures_util::poll!(&mut anchor_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(3), proxy.gate.received.notified())
        .await
        .expect("the anchor must hold the observer while the backlog subscribes");

    let mut backlog_wait = Box::pin(futures_util::future::join_all(
        backlog
            .iter()
            .map(|identity| observer.wait_result(identity, None)),
    ));
    assert!(futures_util::poll!(&mut backlog_wait).is_pending());
    proxy.gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), proxy.gate.received.notified())
        .await
        .expect("the first batch in the backlog snapshot must reach the proxy");

    assert_eq!(
        observer
            .wait_result(&ready, Some(Duration::from_secs(3)))
            .await
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT),
    );
    drop((anchor_wait, backlog_wait));
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_all_rows_arrived_so_pending_subscription_does_not_hold_the_tail() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let pending = dispatch_identity(&user, "pending");
    let terminal = dispatch_identity(&user, "terminal");
    let ready = dispatch_identity(&user, "ready");
    for identity in [&pending, &terminal, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    for identity in [&terminal, &ready] {
        assert!(
            producer
                .deliver_result(identity, "edge", OBSERVED_RECEIPT)
                .await
                .unwrap()
        );
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowTerminator(BATCHED_RESULT_SELECT),
    )
    .await;
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = Arc::new(DatabaseEdgeDispatchService::from_shared(&observed));
    let (publish, published) = tokio::sync::oneshot::channel();
    let batch = tokio::spawn({
        let observer = observer.clone();
        async move {
            tokio::join!(
                observer.wait_result(&pending, Some(Duration::from_secs(20))),
                async {
                    let result = observer
                        .wait_result(&terminal, Some(Duration::from_secs(20)))
                        .await;
                    publish.send(result).unwrap();
                },
            )
        }
    });
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the proxy is holding only the terminator");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), published)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT),
    );
    // No new subscription is present during the checkout. The response has
    // provided both rows, so the connection must be reusable or discarded
    // independently of contention, within much less than a SQL turn.
    tokio::time::timeout(
        Duration::from_millis(900),
        sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()").fetch_one(observed.get()),
    )
    .await
    .expect("a live pending row must not hold the result-set tail")
    .unwrap();
    assert!(
        !batch.is_finished(),
        "the pending tool still owns its caller wait"
    );
    assert_eq!(
        observer
            .wait_result(&ready, Some(Duration::from_secs(3)))
            .await
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT),
    );
    batch.abort();
    assert!(batch.await.unwrap_err().is_cancelled());
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_result_stream_error_preserves_observed_rows() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    for complete in [true, false] {
        let user = unique_suffix();
        let pending = dispatch_identity(&user, "pending");
        let other = dispatch_identity(&user, "other");
        producer
            .admit_dispatch(&pending, "edge", &serde_json::json!({}))
            .await
            .unwrap();
        if complete {
            producer
                .admit_dispatch(&other, "edge", &serde_json::json!({}))
                .await
                .unwrap();
            assert!(
                producer
                    .deliver_result(&other, "edge", OBSERVED_RECEIPT)
                    .await
                    .unwrap()
            );
        }
        let proxy = ResponseProxy::start(
            settings.host.clone(),
            settings.port,
            Boundary::RowTerminatorError(BATCHED_RESULT_SELECT),
        )
        .await;
        let observed = proxy_pool(&settings, proxy.port).await;
        let observer = DatabaseEdgeDispatchService::from_shared(&observed);
        let budget = Some(Duration::from_secs(3));
        let mut pending_wait = Box::pin(observer.wait_result(&pending, budget));
        let mut other_wait = Box::pin(observer.wait_result(&other, budget));
        assert!(futures_util::poll!(&mut pending_wait).is_pending());
        assert!(futures_util::poll!(&mut other_wait).is_pending());
        tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
            .await
            .expect("MatrixOne sent the result rows");
        proxy.gate.release.notify_one();
        if complete {
            assert_eq!(other_wait.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
        } else {
            let error = other_wait
                .await
                .expect_err("the unread identity had no clean EOF");
            assert!(error.contains("injected result stream error"), "{error}");
        }
        assert!(
            producer
                .deliver_result(&pending, "edge", OBSERVED_RECEIPT)
                .await
                .unwrap()
        );
        assert_eq!(
            pending_wait.await.unwrap().as_deref(),
            Some(OBSERVED_RECEIPT)
        );

        observed.get().close().await;
        sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
            .bind(user)
            .execute(pool.get())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_rechecks_an_observed_key_before_its_batch_finishes() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let known = dispatch_identity(&user, "known");
    let unseen = dispatch_identity(&user, "unseen");
    producer
        .admit_dispatch(&known, "edge", &serde_json::json!({}))
        .await
        .unwrap();
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowTerminator(BATCHED_RESULT_SELECT),
    )
    .await;
    let observed = proxy_pool_with_limit(&settings, proxy.port, 3).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);
    let mut result = Box::pin(observer.wait_result(&known, Some(Duration::from_secs(3))));
    let mut other = Box::pin(observer.wait_result(&unseen, None));
    assert!(futures_util::poll!(&mut result).is_pending());
    assert!(futures_util::poll!(&mut other).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the pending row arrived before the held stream end");
    assert!(
        producer
            .deliver_result(&known, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    assert_eq!(result.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert!(futures_util::poll!(&mut other).is_pending());
    drop(other);
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
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
        inserts.spawn(async move {
            producer
                .admit_dispatch(&identity, &agent_id, &serde_json::json!({}))
                .await
        });
    }
    while let Some(insert) = inserts.join_next().await {
        insert.unwrap().expect("concurrent dispatch insert");
    }

    let mut waits = tokio::task::JoinSet::new();
    for identity in identities.clone() {
        let waiter = waiter.clone();
        waits.spawn(async move {
            waiter
                .wait_result(&identity, Some(std::time::Duration::from_secs(10)))
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
    svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"test":true}))
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

    svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"test":true}))
        .await
        .expect("insert dispatch");

    assert!(
        !svc.fail_dispatch(&identity, "wrong-agent-id", "forged cancellation")
            .await
            .expect("wrong-owner failure attempt"),
        "a different edge owner must not terminate the dispatch"
    );
    assert_eq!(
        svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"test":true}))
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
        .admit_dispatch(&identity, &agent_id, &serde_json::json!({"test":true}))
        .await
        .expect("failed dispatch replay")
    else {
        panic!("owner failure must create terminal durable evidence");
    };
    assert!(
        result_json.contains("cancelled"),
        "terminal replay must preserve the failure reason"
    );
    assert_eq!(
        svc.deliver_socket_result(&identity, &agent_id, r#"{"output":"late outcome"}"#)
            .await
            .unwrap(),
        EdgeDispatchDelivery::Retired,
    );
    assert_eq!(
        svc.wait_result(&identity, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .as_deref(),
        Some(result_json.as_str()),
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

    svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({}))
        .await
        .expect("insert_dispatch");

    // 200ms timeout — should not be enough to deliver result
    let result = svc
        .wait_result(&identity, Some(std::time::Duration::from_millis(200)))
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
    svc.admit_dispatch(&identity, &agent, &serde_json::json!({}))
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

    svc.admit_dispatch(&identity_a, &agent_a, &serde_json::json!({"owner":"a"}))
        .await
        .expect("insert user A dispatch");
    svc.admit_dispatch(&identity_b, &agent_b, &serde_json::json!({"owner":"b"}))
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
        .wait_result(&identity_a, Some(std::time::Duration::from_secs(1)))
        .await
        .expect("wait owner A result")
        .expect("owner A result");
    let result_b = svc
        .wait_result(&identity_b, Some(std::time::Duration::from_secs(1)))
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

    svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({}))
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
        .wait_result(&identity, Some(std::time::Duration::from_millis(100)))
        .await
        .expect("wait_result after cleanup");
    assert!(result.is_none(), "request should be gone after cleanup");
    assert_eq!(
        svc.deliver_socket_result(&identity, &agent_id, r#"{"done":true}"#)
            .await
            .unwrap(),
        EdgeDispatchDelivery::Retired,
        "a late journal replay must report authoritative absence"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_cleanup_retains_live_run_custody_and_receipts() {
    use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
    require_env();
    let pool = common::setup_pool().await;
    let service = DatabaseEdgeDispatchService::new(pool.get().clone());
    let user = format!("live-custody-{}", unique_suffix());
    let mut run = common::collaborator_fixture::record(&user, "cleanup-session", None);
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at) VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))")
        .bind(&user).bind(&run.session_id).execute(pool.get()).await.unwrap();
    let edge_owner = format!("shared-edge-owner-{}", unique_suffix());
    let invocation = astra_turn_types::ToolInvocationIdentity::new(
        &user,
        &run.session_id,
        &run.run_id,
        "cleanup-chain",
        "cleanup-request",
    )
    .unwrap();
    let identity = EdgeDispatchIdentity::new(
        &edge_owner,
        &run.session_id,
        &run.run_id,
        "cleanup-chain",
        invocation.storage_key(),
    );
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(run.clone())
        .await
        .unwrap();
    service
        .admit_and_claim_direct_dispatch(
            &identity,
            "cleanup-edge",
            &serde_json::json!({"identity":invocation}),
        )
        .await
        .unwrap();
    let sse_identity = EdgeDispatchIdentity::new(
        &user,
        &run.session_id,
        &run.run_id,
        "cleanup-chain",
        "sse-cleanup-request",
    );
    service
        .admit_and_claim_direct_dispatch(
            &sse_identity,
            "cleanup-edge",
            &serde_json::json!({"type":"tool_request", "tool":"bash"}),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE edge_pending_dispatch SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) WHERE user_id = ? AND request_id = ?")
        .bind(&user).bind(&sse_identity.request_id).execute(pool.get()).await.unwrap();
    sqlx::query("UPDATE edge_pending_dispatch SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) WHERE user_id = ? AND request_id = ?")
        .bind(&edge_owner).bind(&identity.request_id).execute(pool.get()).await.unwrap();
    for status in ["running", "waiting", "paused"] {
        sqlx::query("UPDATE agent_runs SET status = ? WHERE user_id = ? AND run_id = ?")
            .bind(status)
            .bind(&user)
            .bind(&run.run_id)
            .execute(pool.get())
            .await
            .unwrap();
        service
            .cleanup_stale(std::time::Duration::from_secs(86_400))
            .await
            .unwrap();
        let status: String = sqlx::query_scalar(
            "SELECT status FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?",
        )
        .bind(&edge_owner)
        .bind(&identity.request_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(status, "dispatched");
        let sse_status: String = sqlx::query_scalar(
            "SELECT status FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?",
        )
        .bind(&user)
        .bind(&sse_identity.request_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(sse_status, "dispatched");
    }
    assert!(
        service
            .deliver_result(&identity, "cleanup-edge", r#"{"output":"done"}"#)
            .await
            .unwrap()
    );
    assert!(
        service
            .deliver_result(&sse_identity, "cleanup-edge", r#"{"output":"done"}"#)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE edge_pending_dispatch SET completed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) WHERE user_id = ? AND request_id = ?")
        .bind(&user).bind(&sse_identity.request_id).execute(pool.get()).await.unwrap();
    sqlx::query("UPDATE edge_pending_dispatch SET completed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) WHERE user_id = ? AND request_id = ?")
        .bind(&edge_owner).bind(&identity.request_id).execute(pool.get()).await.unwrap();
    service
        .cleanup_stale(std::time::Duration::from_secs(86_400))
        .await
        .unwrap();
    assert!(
        service
            .wait_result(&identity, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        service
            .wait_result(&sse_identity, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .is_some()
    );
    run.status = "completed".into();
    sqlx::query(
        "UPDATE agent_runs SET status = ?, updated_at = NOW(6) WHERE user_id = ? AND run_id = ?",
    )
    .bind(&run.status)
    .bind(&user)
    .bind(&run.run_id)
    .execute(pool.get())
    .await
    .unwrap();
    service
        .cleanup_stale(std::time::Duration::from_secs(86_400))
        .await
        .unwrap();
    for retained in [&identity, &sse_identity] {
        assert!(
            service
                .wait_result(retained, Some(std::time::Duration::ZERO))
                .await
                .unwrap()
                .is_some(),
            "recently completed runs retain their old request's results"
        );
    }
    sqlx::query("UPDATE agent_runs SET updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) WHERE user_id = ? AND run_id = ?")
        .bind(&user).bind(&run.run_id).execute(pool.get()).await.unwrap();
    service
        .cleanup_stale(std::time::Duration::from_secs(86_400))
        .await
        .unwrap();
    assert!(
        service
            .wait_result(&identity, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        service
            .wait_result(&sse_identity, Some(std::time::Duration::ZERO))
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
        .bind(&user)
        .bind(&run.run_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(&user)
        .bind(&run.session_id)
        .execute(pool.get())
        .await
        .unwrap();
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
async fn edge_registry_native_targets_filter_before_limit_and_exclude_scoped_rows() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());
    let user_id = format!("user_{}", unique_suffix());

    let mut advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
        astra_runtime_env::RunBinding::edge_developer(
            "/workspace/project",
            &astra_runtime_env::ToolRegistry::builtins(),
        ),
    );
    let rest_only = serde_json::to_string(&advertisement).unwrap();
    let mut websocket = serde_json::to_value(&advertisement).unwrap();
    websocket["protocol_capabilities"] = serde_json::json!({
        (astra_services::multi_agent::edge_registry::WORK_ATTESTATION_CAPABILITY): true
    });
    let builtin_tools = websocket.to_string();
    advertisement.binding.tool_surface.tool_names.clear();
    let native_only = serde_json::to_string(&advertisement).unwrap();

    // Route, workspace, and capability eligibility apply before LIMIT.
    let candidates = (0..25)
        .map(|index| {
            (
                format!("agent-restricted-{index}"),
                &native_only,
                None,
                Some("/workspace/project"),
            )
        })
        .chain([
            ("agent-missing-root".into(), &builtin_tools, None, None),
            (
                "agent-rest".into(),
                &rest_only,
                None,
                Some("/workspace/project"),
            ),
            (
                "agent-scoped".into(),
                &builtin_tools,
                Some("provider-scope"),
                Some("/workspace/provider"),
            ),
            (
                "agent-valid".into(),
                &builtin_tools,
                None,
                Some("/workspace/project"),
            ),
        ]);
    for (agent_id, capabilities, workspace_id, root) in candidates {
        sqlx::query(
            "INSERT INTO edge_agent_registry
             (user_id, registry_id, edge_agent_id, edge_id, worktree_path,
              materialization_id, workspace_id, capabilities_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&user_id)
        .bind(format!("registry-{agent_id}"))
        .bind(&agent_id)
        .bind(format!("edge-{agent_id}"))
        .bind(root)
        .bind(format!("materialization-{agent_id}"))
        .bind(workspace_id)
        .bind(capabilities)
        .execute(&pool)
        .await
        .expect("seed Work target candidate");
    }

    let targets = svc
        .list_native_execution_targets(&user_id, 25)
        .await
        .expect("list native execution targets");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].edge_agent_id, "agent-valid");
    assert_eq!(targets[0].workspace_id, None);
    assert!(targets[0].is_work_execution_target());

    sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ?")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean native target fixture");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_finalized_claim_is_renewed_and_fences_a_third_generation() {
    expired_finalized_claim_survives_displaced_cleanup(false).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_expired_finalized_claim_publishes_after_displaced_cleanup() {
    expired_finalized_claim_survives_displaced_cleanup(true).await;
}

async fn expired_finalized_claim_survives_displaced_cleanup(publish_third: bool) {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let first_pod = DatabaseEdgeRegistryService::new(pool.clone());
    let second_pod = DatabaseEdgeRegistryService::new(common::setup_pool().await.get().clone());
    let third_pod = DatabaseEdgeRegistryService::new(common::setup_pool().await.get().clone());
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
    assert!(third.previous.is_none());
    assert!(
        second_pod
            .unregister_generation(&user_id, &edge_agent_id, "edge-second")
            .await
            .unwrap()
    );
    let state: (i8, Option<String>, Option<String>, i8) = sqlx::query_as(
        "SELECT registration_state, registration_claim_id, registration_previous_edge_id, \
                registration_claim_expires_at > NOW(6) \
         FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (0, third.claim_id.clone(), None, 1));

    if publish_third {
        assert!(third_pod.finalize_registration(&third).await.unwrap());
        assert!(third_pod.release_registration(&third).await.unwrap());
        // Repeated cleanup must also leave the published successor intact.
        assert!(
            !second_pod
                .unregister_generation(&user_id, &edge_agent_id, "edge-second")
                .await
                .unwrap()
        );
        let published = third_pod.list_by_user(&user_id).await.unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].edge_id, "edge-third");
    } else {
        assert!(third_pod.rollback_registration(&third).await.unwrap());
        assert!(third_pod.list_by_user(&user_id).await.unwrap().is_empty());
    }
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
    svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"tool":"bash"}))
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

    let claimed_rows = if !rows1.is_empty() { &rows1 } else { &rows2 };
    assert_eq!(claimed_rows[0].request_id, request_id);

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
        svc.admit_dispatch(&identity, &agent_id, &serde_json::json!({"test":true}))
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
        total_claimed += rows.len();
    }

    assert_eq!(total_claimed, 3, "exactly 3 rows should be claimed total");
    let dispatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM edge_pending_dispatch WHERE user_id = ? AND edge_agent_id = ? AND status = 'dispatched'",
    )
    .bind(&user_id)
    .bind(&agent_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(dispatched, 3);
    // No more pending
    let remaining = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("final poll");
    assert!(remaining.is_empty(), "no rows should remain after claim");
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_slow_reads_with_staggered_deadlines_complete_without_retries() {
    use std::sync::atomic::Ordering;

    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let first = dispatch_identity(&user, "first-ready");
    let second = dispatch_identity(&user, "second-ready");
    for identity in [&first, &second] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            producer
                .deliver_result(identity, "edge", OBSERVED_RECEIPT)
                .await
                .unwrap()
        );
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(1500)),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);

    let mut first_wait = Box::pin(observer.wait_result(&first, Some(Duration::from_secs(20))));
    assert!(futures_util::poll!(&mut first_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the first read is in flight");
    let (first_result, late_result, second_result) = tokio::join!(
        first_wait,
        observer.wait_result(&first, Some(Duration::from_secs(3))),
        observer.wait_result(&second, Some(Duration::from_millis(3_400))),
    );
    assert_eq!(first_result.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert_eq!(late_result.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert_eq!(second_result.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert_eq!(proxy.gate.matched.load(Ordering::SeqCst), 2);
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_retries_a_failed_read_without_its_cancelled_key() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let anchor = dispatch_identity(&user, "blocked-anchor");
    let cancelled = dispatch_identity(&user, "blocked-cancelled");
    let ready = dispatch_identity(&user, "ready");
    for identity in [&anchor, &cancelled, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"blocked-"),
    )
    .await;
    proxy
        .gate
        .continuous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);

    let mut anchor_wait = Box::pin(observer.wait_result(&anchor, None));
    assert!(futures_util::poll!(&mut anchor_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the first batch is stalled");
    let mut cancelled_wait = Box::pin(observer.wait_result(&cancelled, None));
    let mut ready_wait = Box::pin(observer.wait_result(&ready, Some(Duration::from_secs(5))));
    assert!(futures_util::poll!(&mut cancelled_wait).is_pending());
    assert!(futures_util::poll!(&mut ready_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
        .await
        .expect("the two peers share a read while the first batch is deferred");

    drop(cancelled_wait);
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    proxy
        .gate
        .disconnect_on_release
        .store(true, Ordering::SeqCst);
    proxy.gate.release.notify_one();
    assert_eq!(ready_wait.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    drop(anchor_wait);
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_failed_batch_backoff_does_not_block_other_waiters() {
    use std::sync::atomic::Ordering;
    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let broken = dispatch_identity(&user, "broken-retry");
    let bystander = dispatch_identity(&user, "bystander");
    let ready = dispatch_identity(&user, "ready-after-error");
    for identity in [&broken, &bystander, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"broken-retry"),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let observed = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&observed);
    let mut old = Box::pin(observer.wait_result(&broken, None));
    assert!(futures_util::poll!(&mut old).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the failing read is in flight");
    let mut competing = Box::pin(observer.wait_result(&bystander, None));
    assert!(futures_util::poll!(&mut competing).is_pending());
    proxy
        .gate
        .disconnect_on_release
        .store(true, Ordering::SeqCst);
    proxy.gate.release.notify_one();

    tokio::time::timeout(Duration::from_secs(15), async {
        while proxy.gate.disconnects.load(Ordering::SeqCst) < 6 {
            proxy.gate.disconnected.notified().await;
        }
    })
    .await
    .expect("the failed batch reached its longest retry interval");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert_eq!(proxy.gate.disconnects.load(Ordering::SeqCst), 6);
    let (shared, ()) = tokio::join!(
        observer.wait_result(&ready, Some(Duration::from_secs(3))),
        async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while proxy.gate.disconnects.load(Ordering::SeqCst) < 7 {
                    proxy.gate.disconnected.notified().await;
                }
            })
            .await
            .expect("the failed batch retried while the other caller was waiting");
            assert!(
                producer
                    .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
                    .await
                    .unwrap()
            );
        }
    );
    assert_eq!(shared.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    drop((old, competing));
    observed.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_stalled_checkout_does_not_block_other_waiters() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};
    use std::sync::atomic::Ordering;

    let (pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&pool);
    let user = unique_suffix();
    let waiting = dispatch_identity(&user, "checkout-owner");
    let ready = dispatch_identity(&user, "ready-during-checkout");
    for identity in [&waiting, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    let proxy = ResponseProxy::start(settings.host.clone(), settings.port, Boundary::Ping).await;
    proxy.gate.armed.store(false, Ordering::SeqCst);
    let options = MySqlConnectOptions::new()
        .host("127.0.0.1")
        .port(proxy.port)
        .username(&settings.user)
        .password(&settings.password)
        .database(&settings.database)
        .ssl_mode(MySqlSslMode::Disabled);
    let observed = MySqlPoolOptions::new()
        .max_connections(2)
        .test_before_acquire(true)
        .connect_with(options)
        .await
        .unwrap();
    let observer = DatabaseEdgeDispatchService::new(observed.clone());
    let _: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&observed)
        .await
        .unwrap();
    proxy.gate.armed.store(true, Ordering::SeqCst);

    let mut old = Box::pin(observer.wait_result(&waiting, None));
    assert!(futures_util::poll!(&mut old).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the checkout health check is in flight");
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    assert_eq!(
        observer
            .wait_result(&ready, Some(Duration::from_secs(3)))
            .await
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT)
    );
    drop(old);
    observed.close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_stalled_earlier_deadline_does_not_block_ready_waiter() {
    observe_while_other_read_stalled(2, Duration::from_secs(3)).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_urgent_waiter_uses_spare_checkout() {
    observe_while_other_read_stalled(3, Duration::from_millis(950)).await;
}

async fn observe_while_other_read_stalled(max_connections: u32, budget: Duration) {
    let (producer_pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&producer_pool);
    let user = unique_suffix();
    let a = dispatch_identity(&user, "blocked-earlier");
    let b = dispatch_identity(&user, "ready-later");
    for item in [&a, &b] {
        producer
            .admit_dispatch(item, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&b, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"blocked-earlier"),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let pool = proxy_pool_with_limit(&settings, proxy.port, max_connections).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&pool);
    let mut old = Box::pin(observer.wait_result(&a, Some(Duration::from_secs(3))));
    assert!(futures_util::poll!(&mut old).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .unwrap();
    let fresh = observer.wait_result(&b, Some(budget));
    let (_, fresh) = tokio::join!(old, fresh);
    assert_eq!(fresh.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(producer_pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_retries_a_failed_read_without_its_expired_key() {
    let (producer_pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&producer_pool);
    let user = unique_suffix();
    let a = dispatch_identity(&user, "blocked-peer");
    let b = dispatch_identity(&user, "ready-peer");
    for item in [&a, &b] {
        producer
            .admit_dispatch(item, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&b, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"blocked-peer"),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let pool = proxy_pool_with_limit(&settings, proxy.port, 2).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&pool);
    // An expired subscription should leave the retry when the read fails.
    let mut old = Box::pin(observer.wait_result(&a, Some(Duration::from_millis(2_100))));
    let mut ready = Box::pin(observer.wait_result(&b, Some(Duration::from_secs(3))));
    assert!(futures_util::poll!(&mut old).is_pending());
    assert!(futures_util::poll!(&mut ready).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .unwrap();
    let ((), ready) = tokio::join!(
        async {
            assert_eq!(old.await.unwrap(), None);
            proxy
                .gate
                .disconnect_on_release
                .store(true, Ordering::SeqCst);
            proxy.gate.release.notify_one();
        },
        ready
    );
    assert_eq!(ready.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(producer_pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_parallel_stalled_reads_leave_a_checkout_for_writes() {
    let (producer_pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&producer_pool);
    let user = unique_suffix();
    let first = dispatch_identity(&user, "blocked-parallel-first");
    let second = dispatch_identity(&user, "blocked-parallel-second");
    let ready = dispatch_identity(&user, "ready-parallel");
    for identity in [&first, &second, &ready] {
        producer
            .admit_dispatch(identity, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"blocked-parallel-"),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let pool = proxy_pool_with_limit(&settings, proxy.port, 3).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&pool);
    let mut first_wait = Box::pin(observer.wait_result(&first, None));
    assert!(futures_util::poll!(&mut first_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .expect("the first read is stalled");
    let mut second_wait = Box::pin(observer.wait_result(&second, None));
    assert!(futures_util::poll!(&mut second_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(3), proxy.gate.received.notified())
        .await
        .expect("a second read can run while the first is stalled");

    let delivered = tokio::time::timeout(
        Duration::from_secs(2),
        observer.deliver_result(&ready, "edge", OBSERVED_RECEIPT),
    )
    .await
    .expect("a result write still has a checkout")
    .unwrap();
    assert!(delivered);
    assert_eq!(
        observer
            .wait_result(&ready, Some(Duration::from_secs(3)))
            .await
            .unwrap()
            .as_deref(),
        Some(OBSERVED_RECEIPT)
    );
    drop((first_wait, second_wait));
    pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(producer_pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_recovered_backlog_keeps_up_with_caller_deadlines() {
    let (producer_pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&producer_pool);
    let user = unique_suffix();
    let backlog: Vec<_> = (0..2048)
        .map(|i| dispatch_identity(&user, &format!("backlog-{i:04}")))
        .collect();
    for chunk in backlog.chunks(128) {
        let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO edge_pending_dispatch (user_id,session_id,run_id,turn_chain_id,edge_agent_id,request_id,run_owner_user_id,payload_digest,status,result_json) ",
        );
        insert.push_values(chunk, |mut row, item| {
            row.push_bind(&item.user_id)
                .push_bind(&item.session_id)
                .push_bind(&item.run_id)
                .push_bind(&item.turn_chain_id)
                .push_bind("edge")
                .push_bind(&item.request_id)
                .push_bind(&item.user_id)
                .push_bind("digest")
                .push_bind("completed")
                .push_bind(OBSERVED_RECEIPT);
        });
        insert.build().execute(producer_pool.get()).await.unwrap();
    }
    let anchor = dispatch_identity(&user, "backlog-anchor");
    let pending = dispatch_identity(&user, "backlog-bystander");
    let ready = dispatch_identity(&user, "fresh");
    for item in [&anchor, &pending, &ready] {
        producer
            .admit_dispatch(item, "edge", &serde_json::json!({}))
            .await
            .unwrap();
    }
    assert!(
        producer
            .deliver_result(&ready, "edge", OBSERVED_RECEIPT)
            .await
            .unwrap()
    );
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsMatching(BATCHED_RESULT_SELECT, b"backlog-"),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let pool = proxy_pool(&settings, proxy.port).await;
    let observer = Arc::new(DatabaseEdgeDispatchService::from_shared(&pool));
    let mut anchor_wait = Box::pin(observer.wait_result(&anchor, None));
    assert!(futures_util::poll!(&mut anchor_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(backlog.len() + 1));
    let mut tasks = tokio::task::JoinSet::new();
    for item in &backlog {
        let item = item.clone();
        let observer = observer.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            let mut wait = Box::pin(observer.wait_result(&item, Some(Duration::from_secs(15))));
            assert!(futures_util::poll!(&mut wait).is_pending());
            barrier.wait().await;
            wait.await
        });
    }
    barrier.wait().await;
    proxy.gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(8), proxy.gate.received.notified())
        .await
        .unwrap();
    let mut fresh = Box::pin(observer.wait_result(&ready, Some(Duration::from_secs(3))));
    assert!(futures_util::poll!(&mut fresh).is_pending());
    proxy.gate.continuous.store(false, Ordering::SeqCst);
    proxy.gate.armed.store(false, Ordering::SeqCst);
    assert_eq!(fresh.await.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    drop(anchor_wait);
    let mut pending_wait = Box::pin(observer.wait_result(&pending, None));
    assert!(futures_util::poll!(&mut pending_wait).is_pending());
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        let mut done = 0;
        while let Some(result) = tasks.join_next().await {
            if result.unwrap().unwrap().as_deref() == Some(OBSERVED_RECEIPT) {
                done += 1;
            }
        }
        done
    })
    .await
    .unwrap();
    assert_eq!(completed, backlog.len());
    assert!(proxy.gate.matched.load(Ordering::SeqCst) <= 30);
    drop(pending_wait);
    pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(producer_pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_slow_competing_reads_use_the_caller_budget() {
    let (producer_pool, settings) = common::setup_pool_and_settings().await;
    let producer = DatabaseEdgeDispatchService::from_shared(&producer_pool);
    let user = unique_suffix();
    let first = dispatch_identity(&user, "slow-first");
    let second = dispatch_identity(&user, "slow-second");
    for item in [&first, &second] {
        producer
            .admit_dispatch(item, "edge", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            producer
                .deliver_result(item, "edge", OBSERVED_RECEIPT)
                .await
                .unwrap()
        );
    }
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::RowsAfter(BATCHED_RESULT_SELECT, Duration::from_millis(6_350)),
    )
    .await;
    proxy.gate.continuous.store(true, Ordering::SeqCst);
    let pool = proxy_pool(&settings, proxy.port).await;
    let observer = DatabaseEdgeDispatchService::from_shared(&pool);
    let mut first_wait = Box::pin(observer.wait_result(&first, Some(Duration::from_secs(16))));
    assert!(futures_util::poll!(&mut first_wait).is_pending());
    tokio::time::timeout(Duration::from_secs(2), proxy.gate.received.notified())
        .await
        .unwrap();
    let (a, b) = tokio::time::timeout(Duration::from_secs(16), async {
        tokio::join!(first_wait, observer.wait_result(&second, None))
    })
    .await
    .expect("competing slow reads must make progress");
    assert_eq!(a.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert_eq!(b.unwrap().as_deref(), Some(OBSERVED_RECEIPT));
    assert_eq!(proxy.gate.matched.load(Ordering::SeqCst), 2);
    pool.get().close().await;
    sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
        .bind(user)
        .execute(producer_pool.get())
        .await
        .unwrap();
}
