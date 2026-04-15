//! Remote skill API E2E: register remote skill under current auth mode/user system and verify
//! ownership + persistence in `skills_registry`.
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_remote_skill_registration_user_system_integration() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = b.auth_header.as_str();
    let pool = &ctx.pool;

    let skill_name = format!("remote_skill_{}", ctx.suffix);
    let bad_remote_missing_url = format!("{skill_name}_bad_remote_missing_url");
    let bad_local_with_remote = format!("{skill_name}_bad_local_with_remote");
    let skill_version = "1.0.0";
    let remote_url = format!("http://127.0.0.1:18080/{skill_name}/execute");

    // Validation: remote mode must provide remote_url.
    let (st_bad_remote, j_bad_remote) = post_json(
        app,
        "/skills",
        Some(auth),
        json!({
            "skill_name": bad_remote_missing_url,
            "skill_version": skill_version,
            "skill_type": "remote"
        }),
    )
    .await;
    assert_eq!(
        st_bad_remote,
        StatusCode::BAD_REQUEST,
        "remote skill without remote_url should fail: {j_bad_remote}"
    );
    assert_eq!(
        j_bad_remote["detail"].as_str(),
        Some("remote skills require remote_url")
    );

    // Validation: local mode must not provide remote_url.
    let (st_bad_local, j_bad_local) = post_json(
        app,
        "/skills",
        Some(auth),
        json!({
            "skill_name": bad_local_with_remote,
            "skill_version": skill_version,
            "skill_type": "local",
            "skill_code": "local instruction",
            "remote_url": "http://127.0.0.1:18080/illegal"
        }),
    )
    .await;
    assert_eq!(
        st_bad_local,
        StatusCode::BAD_REQUEST,
        "local skill with remote_url should fail: {j_bad_local}"
    );
    assert_eq!(
        j_bad_local["detail"].as_str(),
        Some("local skills must not provide remote_url")
    );

    let (st_reg, reg_j) = post_json(
        app,
        "/skills",
        Some(auth),
        json!({
            "skill_name": skill_name,
            "skill_version": skill_version,
            "skill_type": "remote",
            "remote_url": remote_url,
            "description": "remote skill user-system e2e",
            "metadata": {
                "when_to_use": "when task should be delegated to external service",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string" }
                    },
                    "required": ["task"]
                },
                "output_schema": {
                    "type": "object",
                    "properties": {
                        "result": { "type": "string" }
                    }
                },
                "tags": ["remote", "e2e"],
                "aliases": ["remote-e2e-alias"]
            }
        }),
    )
    .await;
    assert_eq!(
        st_reg,
        StatusCode::CREATED,
        "register remote skill: {reg_j}"
    );
    assert_eq!(
        reg_j["skill_id"].as_str(),
        Some(format!("{skill_name}@{skill_version}").as_str())
    );
    assert_eq!(reg_j["skill_name"].as_str(), Some(skill_name.as_str()));
    assert_eq!(reg_j["version"].as_str(), Some(skill_version));
    assert_eq!(reg_j["metadata"]["skill_type"].as_str(), Some("remote"));
    assert_eq!(
        reg_j["metadata"]["remote_url"].as_str(),
        Some(remote_url.as_str())
    );

    let (st_list, list_j) = get_json(app, "/skills", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "list skills: {list_j}");
    assert!(
        list_j["skills"].as_array().is_some_and(|skills| {
            skills
                .iter()
                .any(|s| s["skill_name"].as_str() == Some(skill_name.as_str()))
        }),
        "registered remote skill should be visible in list: {list_j}"
    );

    let (st_get, get_j) = get_json(app, &format!("/skills/{skill_name}"), Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "get skill by name: {get_j}");
    assert_eq!(get_j["skill_name"].as_str(), Some(skill_name.as_str()));
    assert_eq!(get_j["version"].as_str(), Some(skill_version));
    assert_eq!(get_j["metadata"]["skill_type"].as_str(), Some("remote"));
    assert_eq!(
        get_j["metadata"]["remote_url"].as_str(),
        Some(remote_url.as_str())
    );

    let (st_versions, versions_j) = get_json(
        app,
        &format!("/skills/{skill_name}/versions"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_versions,
        StatusCode::OK,
        "list skill versions by name: {versions_j}"
    );
    assert!(
        versions_j.as_array().is_some_and(|versions| versions
            .iter()
            .any(|v| v["version"].as_str() == Some(skill_version))),
        "registered version should be discoverable: {versions_j}"
    );

    let row = sqlx::query(
        "SELECT created_by, source, status, is_active, \
         IFNULL(CAST(skill_definition AS CHAR), 'null') AS definition_json \
         FROM skills_registry WHERE skill_name = ? AND version = ? \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&skill_name)
    .bind(skill_version)
    .fetch_optional(pool)
    .await
    .expect("select remote skill row");
    let row = row.expect("remote skill row should exist");
    assert_eq!(
        row.try_get::<String, _>("created_by").ok().as_deref(),
        Some(ctx.user_id.as_str()),
        "registered skill owner should map to authenticated user in current auth mode"
    );
    assert_eq!(
        row.try_get::<String, _>("source").ok().as_deref(),
        Some("user")
    );
    assert_eq!(
        row.try_get::<String, _>("status").ok().as_deref(),
        Some("active")
    );
    assert_eq!(
        row.try_get::<i16, _>("is_active").ok(),
        Some(1),
        "remote skill row should be active"
    );
    let definition_json = row
        .try_get::<String, _>("definition_json")
        .expect("definition_json");
    let definition: Value =
        serde_json::from_str(&definition_json).expect("skill_definition should be valid JSON");
    assert_eq!(definition["skill_type"].as_str(), Some("remote"));
    assert_eq!(
        definition["remote_url"].as_str(),
        Some(remote_url.as_str()),
        "remote URL should be persisted in skill_definition"
    );
    assert_eq!(
        definition["input_schema"]["properties"]["task"]["type"]
            .as_str()
            .unwrap_or(""),
        "string"
    );
    assert_eq!(
        definition["output_schema"]["properties"]["result"]["type"]
            .as_str()
            .unwrap_or(""),
        "string"
    );

    let _ = sqlx::query(
        "DELETE FROM skills_registry WHERE skill_name IN (?, ?, ?) AND source = 'user'",
    )
    .bind(&skill_name)
    .bind(&bad_remote_missing_url)
    .bind(&bad_local_with_remote)
    .execute(pool)
    .await;

    ctx.pool.close().await;
}
