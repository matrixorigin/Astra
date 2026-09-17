//! Live MatrixOne integration coverage for multi-agent edge coordination.

use astra_core::SharedPool;
use astra_services::multi_agent::{DatabaseEdgeRegistryService, EdgeRegistryService};
use uuid::Uuid;

mod common;

async fn setup_pool() -> SharedPool {
    common::setup_pool().await
}

async fn cleanup_edge(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, edge_agent_id: &str) {
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn edge_registry_register_twice_keeps_registry_id() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let edge_agent = format!("it-edge-{}", Uuid::new_v4());
    cleanup_edge(&pool, &user, &edge_agent).await;

    let registry = DatabaseEdgeRegistryService::new(pool.clone());
    let first = registry
        .register_or_update(
            &user,
            &edge_agent,
            "transport-a",
            Some("h1"),
            Some("/tmp/a"),
            Some(serde_json::json!({ "k": 1 })),
            Some("ws-1"),
        )
        .await
        .expect("register first edge projection");

    let second = registry
        .register_or_update(
            &user,
            &edge_agent,
            "transport-b",
            Some("h2"),
            Some("/tmp/b"),
            Some(serde_json::json!({ "k": 2 })),
            Some("ws-1"),
        )
        .await
        .expect("update edge projection");

    assert_eq!(second.registry_id, first.registry_id);
    assert_eq!(second.edge_id, "transport-b");
    assert_eq!(second.hostname.as_deref(), Some("h2"));

    registry
        .heartbeat(&user, &edge_agent, "transport-b", None)
        .await
        .expect("heartbeat");

    cleanup_edge(&pool, &user, &edge_agent).await;
}
