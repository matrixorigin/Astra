//! `GET /` and `GET /health` on the real app (no auth) — service metadata + DB + persist counters.

use axum::http::StatusCode;
use serde_json::Value;

use super::harness::{bootstrap, get_json};

pub async fn run_meta_root_and_health() {
    let b = bootstrap().await;
    let app = &b.ctx.app;

    let (st_root, root): (StatusCode, Value) = get_json(app, "/", None, &[]).await;
    assert_eq!(st_root, StatusCode::OK);
    assert!(
        root["name"].as_str().is_some_and(|s| !s.is_empty()),
        "root.name: {root}"
    );
    assert!(
        root["version"].as_str().is_some_and(|s| !s.is_empty()),
        "root.version: {root}"
    );

    let (st_h, health) = get_json(app, "/health", None, &[]).await;
    assert_eq!(st_h, StatusCode::OK, "health: {health}");
    assert_eq!(health["status"].as_str(), Some("healthy"));
    assert_eq!(health["database"].as_str(), Some("connected"));

    assert!(
        health["persist_ok"].as_u64().is_some(),
        "persist_ok counter: {health}"
    );
    assert!(
        health["persist_fail"].as_u64().is_some(),
        "persist_fail counter: {health}"
    );

    b.ctx.pool.close().await;
}
