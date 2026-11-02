//! Cancellation at real MySQL protocol boundaries, using an isolated live test database.
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test edge_registry_cancellation_db_it -- --ignored

mod common;
use common::mysql_response_proxy::{Boundary, ResponseProxy, proxy_pool as audit_proxy_pool};

use std::time::Duration;

use astra_services::multi_agent::{DatabaseEdgeRegistryService, EdgeRegistryService};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};

async fn cancelled_publication(boundary: Boundary) {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let observer = shared.get();
    let user = format!("registry-cancel-{}", uuid::Uuid::new_v4());
    let registry = DatabaseEdgeRegistryService::new(observer.clone());
    let lease = registry
        .register_or_update_with_lease(&user, "cancel-edge", "transport", None, None, None, None)
        .await
        .unwrap();
    assert!(registry.finalize_registration(&lease).await.unwrap());

    let proxy = ResponseProxy::start(settings.host, settings.port, boundary).await;
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(
            MySqlConnectOptions::new()
                .host("127.0.0.1")
                .port(proxy.port)
                .username(&settings.user)
                .password(&settings.password)
                .database(&settings.database)
                // The loopback fixture must see MySQL framing; production TLS is
                // unchanged. Never use this fixture for a non-test database.
                .ssl_mode(MySqlSslMode::Disabled),
        )
        .await
        .unwrap();
    let original: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let pending = tokio::spawn({
        let registry = DatabaseEdgeRegistryService::new(pool.clone());
        let lease = lease.clone();
        async move { registry.release_registration(&lease).await }
    });
    let reached =
        tokio::time::timeout(Duration::from_secs(30), proxy.gate.received.notified()).await;
    pending.abort();
    let cancelled = pending.await;
    // Deliver the original server response intact. Ordinary pool.begin()
    // wrongly reuses this checkout: SQLx never constructed its Transaction
    // (or incremented transaction_depth) before the BEGIN await was cancelled.
    proxy.gate.release.notify_one();

    let replacement = sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()")
        .fetch_one(&pool)
        .await;
    // Retry from an independent pool before closing the tested pool: closing
    // it first would itself release leaked locks and conceal the regression.
    let retry = tokio::time::timeout(Duration::from_secs(15), async {
        let first = registry.release_registration(&lease).await?;
        let replay = registry.release_registration(&lease).await?;
        registry
            .heartbeat(&user, "cancel-edge", "transport", None)
            .await
            .map_err(|error| format!("heartbeat after retry: {error}"))?;
        let healthy = DatabaseEdgeRegistryService::new(pool.clone());
        let replay_on_replacement = healthy.release_registration(&lease).await?;
        let reused: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&pool)
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((first, replay, replay_on_replacement, reused))
    })
    .await;
    pool.close().await;
    drop(proxy);
    sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ?")
        .bind(&user)
        .execute(observer)
        .await
        .unwrap();

    reached.expect("the real server must execute the gated statement before cancellation");
    assert!(cancelled.is_err_and(|error| error.is_cancelled()));
    let replacement = replacement.unwrap();
    assert_ne!(
        original, replacement,
        "cancelled transaction must discard its physical connection"
    );
    assert_eq!(
        retry
            .expect("cancelled publication must release its row lock and remain retryable")
            .unwrap(),
        (true, true, true, replacement),
        "publication must remain idempotent and completed commits must reuse the healthy connection"
    );
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn publication_cancelled_before_begin_ack_discards_connection() {
    cancelled_publication(Boundary::Begin).await;
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn publication_cancelled_after_row_write_releases_lock() {
    cancelled_publication(Boundary::Write(
        b"UPDATE edge_agent_registry SET last_heartbeat_at = last_heartbeat_at",
    ))
    .await;
}

// Pause a real SELECT response after its server-side execution. The independent
// writer then commits while the public Audit read is consuming that statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn audit_tool_snapshot_survives_concurrent_first_run_event() {
    use astra_services::session_audit::{DatabaseSessionAuditService, SessionAuditService};
    use astra_services::{DatabaseRunStateStore, RunStateStore};
    use serde_json::json;
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status,title) VALUES (?,?,'active','audit snapshot')")
        .bind(&user).bind(&session).execute(shared.get()).await.unwrap();
    let mut run = common::collaborator_fixture::record(&user, &session, None);
    run.events = vec![json!({"event_type":"run_started","data":{}})];
    let run_id = run.run_id.clone();
    let store = DatabaseRunStateStore::new(shared.clone());
    store.insert_run(run).await.unwrap();
    sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,tool_call_id,meta_tool_name,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'tool_call_completed',?,'call','bash',CAST('{}' AS JSON),?,?)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&user).bind(&session).bind(&run_id)
        .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::Rows(b"WITH selected_runs AS"),
    )
    .await;
    let test_pool = audit_proxy_pool(&settings, proxy.port).await;
    let audit = DatabaseSessionAuditService::new(settings).with_pool(test_pool.clone());
    let pending = tokio::spawn({
        let audit = audit.clone();
        let user = user.clone();
        let session = session.clone();
        async move { audit.get_tool_analytics(&user, &session).await }
    });
    tokio::time::timeout(Duration::from_secs(30), proxy.gate.received.notified())
        .await
        .expect("tool SELECT reached server");
    store.append_events_batch(&user, &session, &run_id, &[
        json!({"event_type":"tool_call","data":{"name":"bash","id":"call"}}),
        json!({"event_type":"tool_call_end","data":{"name":"bash","id":"call","success":true}}),
        json!({"event_type":"tool_call","data":{"name":"rg","id":"later-call"}}),
        json!({"event_type":"tool_call_end","data":{"name":"rg","id":"later-call","success":true}}),
    ]).await.unwrap();
    proxy.gate.release.notify_one();
    let during = pending.await.unwrap().unwrap();
    let after = audit.get_tool_analytics(&user, &session).await.unwrap();
    assert_eq!(during.len(), 1);
    assert_eq!((during[0].name.as_str(), during[0].call_count), ("bash", 1));
    assert_eq!(after.len(), 2);
    assert_eq!(
        after
            .iter()
            .find(|tool| tool.name == "bash")
            .unwrap()
            .call_count,
        1
    );
    assert_eq!(
        after
            .iter()
            .find(|tool| tool.name == "rg")
            .unwrap()
            .call_count,
        1
    );
    test_pool.close().await;
    drop(proxy);
    for table in [
        "agent_run_events",
        "agent_run_openings",
        "agent_runs",
        "agent_session_lifecycle_fences",
        "agent_events",
        "agent_sessions",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
    }
}

async fn cancelled_audit_approval_query(boundary: Boundary) {
    use astra_services::session_audit::{DatabaseSessionAuditService, SessionAuditService};
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status,title) VALUES (?,?,'active','audit cancellation')")
        .bind(&user).bind(&session).execute(shared.get()).await.unwrap();
    let proxy = ResponseProxy::start(settings.host.clone(), settings.port, boundary).await;
    let pool = audit_proxy_pool(&settings, proxy.port).await;
    let original: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool.get())
        .await
        .unwrap();
    let audit = DatabaseSessionAuditService::new(settings).with_pool(pool.clone());
    let pending = tokio::spawn({
        let user = user.clone();
        let session = session.clone();
        async move { audit.get_summary(&user, &session).await }
    });
    tokio::time::timeout(Duration::from_secs(30), proxy.gate.received.notified())
        .await
        .expect("approval SELECT reached server");
    assert!(
        !pending.is_finished(),
        "reader must still be consuming the response"
    );
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let replacement: u64 = tokio::time::timeout(
        Duration::from_secs(10),
        sqlx::query_scalar("SELECT CONNECTION_ID()").fetch_one(pool.get()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(
        original, replacement,
        "cancelled in-flight read must discard its connection"
    );
    proxy.gate.release.notify_one();
    pool.close().await;
    drop(proxy);
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ?")
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn audit_approval_query_cancellation_releases_single_pool_slot() {
    cancelled_audit_approval_query(Boundary::Rows(
        b"SELECT COUNT(CASE WHEN kind = 'approval_required'",
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn audit_approval_terminator_cancellation_releases_single_pool_slot() {
    cancelled_audit_approval_query(Boundary::RowTerminator(
        b"SELECT COUNT(CASE WHEN kind = 'approval_required'",
    ))
    .await;
}
