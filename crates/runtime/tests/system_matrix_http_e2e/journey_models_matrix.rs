//! Authenticated Offering and Model Access projections.

use axum::http::StatusCode;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_models_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let (st_invalid_cursor, invalid_cursor) =
        get_json(&ctx.app, "/models?after_provider=only-one", Some(auth), &[]).await;
    assert_eq!(
        st_invalid_cursor,
        StatusCode::BAD_REQUEST,
        "partial catalog cursors must fail closed: {invalid_cursor}"
    );

    for suffix in ["page-a", "page-b"] {
        let (status, body) = post_json(
            &ctx.app,
            "/models",
            Some(auth),
            serde_json::json!({
                "name": format!("catalog-{}-{}", ctx.suffix, suffix),
                "provider": "mock",
                "context_window": 200000,
                "api_key": "unused",
                "base_url": "http://127.0.0.1:1"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed paginated model: {body}");
    }
    sqlx::query(
        "UPDATE infra_llm_models SET is_active = 1, updated_at = NOW(6) WHERE model_name LIKE ?",
    )
    .bind(format!("catalog-{}-%", ctx.suffix))
    .execute(&ctx.pool)
    .await
    .expect("activate paginated catalog fixtures");

    let (st_models, models_j) = get_json(&ctx.app, "/models?limit=1", Some(auth), &[]).await;
    assert_eq!(st_models, StatusCode::OK, "models: {models_j}");
    let first_items = models_j["items"].as_array().expect("GET /models items");
    assert_eq!(
        first_items.len(),
        1,
        "limit must bound one page: {models_j}"
    );
    assert_eq!(models_j["limit"], serde_json::json!(1));
    let total = models_j["total"].as_u64().expect("catalog total");
    assert!(
        total >= 3,
        "seeded catalog must exercise continuation: {models_j}"
    );
    let first_revision = models_j["catalog_revision"].clone();
    let cursor = models_j["next_cursor"]
        .as_object()
        .expect("continuation cursor");
    let cursor_path = format!(
        "/models?limit=1&after_provider={}&after_name={}&after_offering_id={}",
        cursor["provider"].as_str().unwrap(),
        cursor["model_name"].as_str().unwrap(),
        cursor["model_id"].as_str().unwrap()
    );
    let (st_models_next, models_next) = get_json(&ctx.app, &cursor_path, Some(auth), &[]).await;
    assert_eq!(
        st_models_next,
        StatusCode::OK,
        "models continuation: {models_next}"
    );
    assert_eq!(models_next["items"].as_array().unwrap().len(), 1);
    assert_eq!(models_next["total"], serde_json::json!(total));
    assert_eq!(models_next["limit"], serde_json::json!(1));
    assert_eq!(models_next["catalog_revision"], first_revision);
    assert_ne!(
        models_next["items"][0]["offering_id"], first_items[0]["offering_id"],
        "seek continuation must not repeat the boundary Offering"
    );

    let (st_access, access_j) = get_json(&ctx.app, "/model-access?limit=1", Some(auth), &[]).await;
    assert_eq!(st_access, StatusCode::OK, "model-access: {access_j}");
    let accesses = access_j["accesses"]
        .as_array()
        .expect("model-access accesses array");
    assert_eq!(accesses.len(), 1, "self-hosted server access: {access_j}");
    assert_eq!(accesses[0]["id"], "self-hosted");
    assert_eq!(accesses[0]["kind"], "self_hosted");
    assert_eq!(accesses[0]["execution_placement"], "server");
    assert_eq!(accesses[0]["status"], "ready");
    assert_eq!(accesses[0]["reason"], serde_json::Value::Null);
    assert_eq!(accesses[0]["usable"], true);
    assert_eq!(accesses[0]["retry_after_seconds"], serde_json::Value::Null);

    let effective_offerings = access_j["offerings"]
        .as_array()
        .expect("model-access offerings array");
    assert!(
        !effective_offerings.is_empty(),
        "seeded MatrixOne catalog must expose an effective Offering: {access_j}"
    );
    assert!(effective_offerings.iter().all(|offering| {
        offering["is_active"] == true
            && offering["access_id"] == "self-hosted"
            && offering["execution_placement"] == "server"
            && offering["offering_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
    }));
    let access_total = accesses[0]["available_model_count"]
        .as_u64()
        .expect("global active Offering count");
    assert_eq!(access_j["limit"], serde_json::json!(1));
    assert_eq!(access_j["total"], serde_json::json!(access_total));
    assert!(
        access_total >= 2,
        "active catalog must exercise Model Access continuation: {access_j}"
    );
    assert_eq!(effective_offerings.len(), 1, "access page limit must apply");
    let default_offering_id = access_j["default_offering_id"]
        .as_str()
        .expect("non-empty catalog has a Server-governed default Offering");
    assert!(
        effective_offerings
            .iter()
            .any(|offering| offering["offering_id"] == default_offering_id),
        "default must reference an effective Offering: {access_j}"
    );
    let catalog_revision = access_j["catalog_revision"]
        .as_str()
        .expect("catalog revision");
    assert!(catalog_revision.starts_with("sha256:"));

    let access_cursor = access_j["next_cursor"].as_object().expect("access cursor");
    let access_cursor_path = format!(
        "/model-access?limit=1&after_provider={}&after_name={}&after_offering_id={}",
        access_cursor["provider"].as_str().unwrap(),
        access_cursor["model_name"].as_str().unwrap(),
        access_cursor["model_id"].as_str().unwrap()
    );
    let (st_access_again, access_again) =
        get_json(&ctx.app, &access_cursor_path, Some(auth), &[]).await;
    assert_eq!(st_access_again, StatusCode::OK, "model-access repeat");
    assert_eq!(
        access_again["catalog_revision"], access_j["catalog_revision"],
        "all pages must share stable catalog revision"
    );
    assert_eq!(access_again["limit"], serde_json::json!(1));
    assert_eq!(access_again["total"], access_j["total"]);
    assert_eq!(
        access_again["default_offering_id"],
        serde_json::Value::Null,
        "continuation pages must not redeclare the global default"
    );
    assert_eq!(access_again["offerings"].as_array().unwrap().len(), 1);
    assert_ne!(
        access_again["offerings"][0]["offering_id"], effective_offerings[0]["offering_id"],
        "Model Access continuation must not repeat its boundary Offering"
    );

    b.ctx.close().await;
}
