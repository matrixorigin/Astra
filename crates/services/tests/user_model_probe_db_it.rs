//! Real model service + isolated DB + strict, non-billable provider fixture.
//! Run with ASTRA_TEST_DB_IT=1, ASTRA_TEST_DATABASE=$ASTRA_DATABASE,
//! ASTRA_ALLOW_INSECURE_DEFAULTS=1 and ASTRA_BYOK_DEEPSEEK_BASE_URL set to
//! an unused loopback HTTP origin (e.g. http://127.0.0.1:18994).
//! Explicitly selected with --features external-contract-tests; ordinary
//! online lanes do not provide this operator-only endpoint override.
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;

use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService,
    models::{UserModelCreateRequestData, UserModelUpdateRequestData},
};
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires isolated MatrixOne DB and loopback DeepSeek fixture override"]
async fn user_model_create_rotate_and_probe_enforce_provider_wire_contract() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    assert_eq!(
        std::env::var("ASTRA_ALLOW_INSECURE_DEFAULTS").as_deref(),
        Ok("1")
    );
    let base = std::env::var("ASTRA_BYOK_DEEPSEEK_BASE_URL").expect("loopback fixture origin");
    let url = reqwest::Url::parse(&base).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let fail_probe = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[derive(Default)]
    struct ConnectivityHold {
        // 1/2 hold connectivity success/failure; 3 holds thinking; 0 passes.
        next: std::sync::atomic::AtomicU8,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    let hold = Arc::new(ConnectivityHold::default());
    let app = Router::new()
        .route(
            "/chat/completions",
            post(
                |axum::extract::State((fail, requests, hold)): axum::extract::State<(
                    Arc<std::sync::atomic::AtomicBool>,
                    Arc<std::sync::atomic::AtomicUsize>,
                    Arc<ConnectivityHold>,
                )>,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Each simulated upstream enforces its own documented
                    // field; do not reuse the serializer under test here.
                    let (limit, forbidden) = match body["model"].as_str() {
                        Some("deepseek-chat") => ("max_tokens", "max_completion_tokens"),
                        _ => ("max_completion_tokens", "max_tokens"),
                    };
                    let status = if headers
                        .get("authorization")
                        .is_none_or(|v| v != "Bearer valid-key" && v != "Bearer rotated-key")
                        || headers
                            .get("x-provider-token")
                            .is_some_and(|v| v != "valid-fixture-token")
                    {
                        StatusCode::UNAUTHORIZED
                    } else if body["model"] != "deepseek-chat" && body["model"] != "o3" {
                        StatusCode::NOT_FOUND
                    } else if (body[limit] != 32 && body[limit] != 1024)
                        || body.get(forbidden).is_some()
                    {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    if status == StatusCode::OK {
                        let outcome = hold.next.load(std::sync::atomic::Ordering::SeqCst);
                        let matches = (body[limit] == 32 && matches!(outcome, 1 | 2))
                            || (body[limit] == 1024 && outcome == 3);
                        if matches
                            && hold
                                .next
                                .compare_exchange(
                                    outcome,
                                    0,
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                )
                                .is_ok()
                        {
                            hold.started.notify_one();
                            hold.release.notified().await;
                            if outcome == 2 {
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    Json(json!({"error":"held connectivity failure"})),
                                );
                            }
                        }
                    }
                    if status == StatusCode::OK && body[limit] == 1024 {
                        if fail.load(std::sync::atomic::Ordering::SeqCst) {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error":"fixture failure"})),
                            );
                        }
                        assert!(body.get("temperature").is_none());
                        assert!(body.get("enable_thinking").is_none());
                        assert!(body.get("reasoning_effort").is_none());
                        let mut message = json!({"content":"391"});
                        if body["thinking"]["type"] == "enabled" {
                            message["reasoning_content"] = json!("multiplication");
                        }
                        return (
                            status,
                            Json(json!({"choices":[{"message":message,"finish_reason":"stop"}]})),
                        );
                    }
                    (
                        status,
                        Json(json!({"error":{"message":"strict fixture rejection"}})),
                    )
                },
            ),
        )
        .route(
            "/v1/messages",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    let status = if headers
                        .get("x-api-key")
                        .is_none_or(|v| v != "valid-key" && v != "rotated-key")
                    {
                        StatusCode::UNAUTHORIZED
                    } else if body["model"] != "claude-sonnet-4-5" {
                        StatusCode::NOT_FOUND
                    } else if headers
                        .get("anthropic-version")
                        .is_none_or(|v| v != "2023-06-01")
                        || body["max_tokens"] != 32
                        || body.get("max_completion_tokens").is_some()
                    {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        Json(json!({"error":{"message":"strict fixture rejection"}})),
                    )
                },
            ),
        )
        .with_state((fail_probe.clone(), requests.clone(), hold.clone()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    let fixture = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (pool, settings) = common::setup_pool_and_settings().await;
    let encryptor = FernetTokenEncryptor::new("provider-wire-test-only-key").unwrap();
    let service = DatabaseModelService::new(settings.clone(), Arc::new(encryptor.clone()))
        .with_pool(pool.clone());
    let owner = uuid::Uuid::new_v4().to_string();
    let make_request = |name: &str, model: &str, key: &str| UserModelCreateRequestData {
        name: name.into(),
        provider: "deepseek".into(),
        model: model.into(),
        base_url: None,
        api_key: key.into(),
        context_window: 128000,
        is_default: true,
    };
    for (model, key) in [
        ("deepseek-chat", "invalid-key"),
        ("missing-model", "valid-key"),
    ] {
        let error = service
            .create_user_model(owner.clone(), make_request("rejected", model, key))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            service
                .list_user_models(owner.clone())
                .await
                .unwrap()
                .is_empty(),
            "failed create persisted data"
        );
    }
    let created = service
        .create_user_model(
            owner.clone(),
            make_request("fixture", "deepseek-chat", "valid-key"),
        )
        .await
        .unwrap();
    // Fixed official endpoints are not user-overridable. Seed their *test row*
    // with this loopback fixture to exercise the real rotate/probe service paths
    // without adding a production transport bypass or spending a live API key.
    for (provider, model) in [
        ("deepseek", "deepseek-chat"),
        ("openai", "o3"),
        ("anthropic", "claude-sonnet-4-5"),
    ] {
        sqlx::query("UPDATE user_llm_models SET provider = ?, model_name = ?, api_key_encrypted = ? WHERE user_id = ? AND model_id = ?")
            .bind(provider).bind(model).bind(encryptor.encrypt("valid-key").unwrap()).bind(&owner).bind(&created.model_id).execute(pool.get()).await.unwrap();
        let checked = service
            .check_user_model(owner.clone(), created.model_id.clone())
            .await
            .unwrap();
        if provider == "deepseek" {
            assert_eq!(
                checked.thinking_probe.as_ref().unwrap().capability,
                astra_services::models::ThinkingCapability::Both
            );
            assert!(checked.thinking_probe.as_ref().unwrap().error.is_none());
            let execution = astra_services::models::revalidate_admitted_model_execution(
                &settings,
                &encryptor,
                &owner,
                &created.model_id,
                Some(pool.get()),
            )
            .await
            .unwrap();
            assert_eq!(
                execution.thinking_capability,
                Some(astra_services::models::ThinkingCapability::Both)
            );
            fail_probe.store(true, std::sync::atomic::Ordering::SeqCst);
            let failed_check = service
                .check_user_model(owner.clone(), created.model_id.clone())
                .await
                .unwrap();
            let observation = failed_check.thinking_probe.unwrap();
            assert_eq!(
                observation.capability,
                astra_services::models::ThinkingCapability::Both
            );
            assert!(observation.error.is_some());
            let retained = astra_services::models::revalidate_admitted_model_execution(
                &settings,
                &encryptor,
                &owner,
                &created.model_id,
                Some(pool.get()),
            )
            .await
            .unwrap();
            assert_eq!(
                retained.thinking_capability,
                Some(astra_services::models::ThinkingCapability::Both)
            );
            fail_probe.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let before: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        let error = service
            .update_user_model(
                owner.clone(),
                created.model_id.clone(),
                UserModelUpdateRequestData {
                    api_key: Some("invalid-key".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST, "{provider}: {error:?}");
        let after: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(before, after, "failed rotation overwrote credential");
        service
            .update_user_model(
                owner.clone(),
                created.model_id.clone(),
                UserModelUpdateRequestData {
                    api_key: Some("rotated-key".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let after: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(encryptor.decrypt(&after).unwrap(), "rotated-key");
        assert!(
            service
                .get_user_model(owner.clone(), created.model_id.clone())
                .await
                .unwrap()
                .thinking_probe
                .is_none(),
            "rotation must invalidate the previous observation"
        );
        service
            .check_user_model(owner.clone(), created.model_id.clone())
            .await
            .unwrap();
    }
    // Only current, configuration-bound observations authorize controls and
    // survive an inconclusive check while supplying every read projection.
    use astra_services::models::{ModelCreateRequestData, QuirksData, ThinkingCapability};
    let patch = |key: Option<&str>, active| astra_services::models::ModelUpdateRequestData {
        api_key: key.map(str::to_owned),
        base_url: None,
        provider: None,
        description: None,
        context_window: None,
        max_completion_tokens: None,
        input_modalities: None,
        output_modalities: None,
        supported_parameters: None,
        pricing: None,
        architecture: None,
        tags: None,
        is_active: active,
        quirks: None,
    };
    let request = ModelCreateRequestData {
        name: "admin-fixture".into(),
        provider: "deepseek".into(),
        api_key: "valid-key".into(),
        base_url: Some(base),
        description: None,
        context_window: Some(128000),
        max_completion_tokens: None,
        input_modalities: vec!["text".into()],
        output_modalities: vec!["text".into()],
        supported_parameters: vec![],
        pricing: None,
        architecture: None,
        tags: vec![],
        quirks: Some(QuirksData {
            wire_model_name: Some("deepseek-chat".into()),
            ..Default::default()
        }),
    };
    service
        .create_model(owner.clone(), request.clone())
        .await
        .unwrap();
    let mut verified_request = request.clone();
    verified_request.name = "admin-verified-fixture".into();
    service
        .create_model(owner.clone(), verified_request)
        .await
        .unwrap();
    fail_probe.store(false, std::sync::atomic::Ordering::SeqCst);
    service
        .check_model("admin-verified-fixture".into())
        .await
        .unwrap();
    let assert_views = |expected, expected_error: Option<bool>| {
        let service = &service;
        let settings = &settings;
        let encryptor = &encryptor;
        let pool = &pool;
        let owner = &owner;
        let requests = &requests;
        async move {
            let before = requests.load(std::sync::atomic::Ordering::SeqCst);
            let admin = service.get_model("admin-fixture".into()).await.unwrap();
            assert_eq!(admin.thinking_capability, expected);
            assert_eq!(
                admin
                    .thinking_probe
                    .as_ref()
                    .map(|probe| probe.error.is_some()),
                expected_error
            );
            let catalog = service.list_models(owner.clone(), true).await.unwrap();
            let entry = catalog
                .iter()
                .find(|entry| entry.name == "admin-fixture")
                .unwrap();
            assert_eq!(entry.thinking_capability, expected);
            let resolved = service
                .revalidate_model_offering(entry.offering_id.clone())
                .await
                .unwrap();
            assert_eq!(resolved.model.thinking_capability, expected);
            let memory = astra_services::models::resolve_memory_offerings(
                settings,
                encryptor,
                owner,
                Some(pool.get()),
            )
            .await
            .unwrap();
            let index = memory
                .iter()
                .position(|item| item.model.model_name == "admin-fixture")
                .unwrap();
            assert_eq!(memory[index].model.thinking_capability, expected);
            if expected.is_none() {
                let verified = memory
                    .iter()
                    .position(|item| item.model.model_name == "admin-verified-fixture")
                    .unwrap();
                assert!(
                    verified < index,
                    "an unchecked model cannot win the capability priority tier"
                );
            }
            assert_eq!(
                requests.load(std::sync::atomic::Ordering::SeqCst),
                before,
                "model reads must not probe the provider"
            );
        }
    };
    assert_views(None, None).await;
    for (fail, expected) in [
        (true, None),
        (false, Some(ThinkingCapability::Both)),
        (true, Some(ThinkingCapability::Both)),
    ] {
        fail_probe.store(fail, std::sync::atomic::Ordering::SeqCst);
        let checked = service.check_model("admin-fixture".into()).await.unwrap();
        assert_eq!(checked.thinking_capability, expected);
        let result = checked.thinking_probe.unwrap();
        assert_eq!(
            result.capability,
            expected.unwrap_or(ThinkingCapability::None)
        );
        assert_eq!(result.error.is_some(), fail);
        assert_views(expected, Some(fail)).await;
    }
    let raw: String = sqlx::query_scalar("SELECT CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'")
        .fetch_one(pool.get()).await.unwrap();
    let snapshot: Value = serde_json::from_str(&raw).unwrap();
    for (field, value) in [
        ("revision", json!(1)),
        ("identity", json!("different-config")),
        ("protocol", json!("moonshot")),
    ] {
        let mut invalid = snapshot.clone();
        invalid[field] = value;
        // Stale observations cannot authorize controls through any read projection.
        sqlx::query("UPDATE infra_llm_models SET thinking_probe_json = ? WHERE model_name = 'admin-fixture'")
            .bind(invalid.to_string()).execute(pool.get()).await.unwrap();
        assert_views(None, None).await;
    }
    sqlx::query(
        "UPDATE infra_llm_models SET thinking_probe_json = '{}' WHERE model_name = 'admin-fixture'",
    )
    .execute(pool.get())
    .await
    .unwrap();
    assert_views(None, None).await;
    // Hold an actual connectivity request, rotate through the public service,
    // then release either success or failure from the superseded configuration.
    fail_probe.store(false, std::sync::atomic::Ordering::SeqCst);
    for outcome in [1, 2] {
        let before = requests.load(std::sync::atomic::Ordering::SeqCst);
        hold.next
            .store(outcome, std::sync::atomic::Ordering::SeqCst);
        let check = service.check_model("admin-fixture".into());
        let rotation = async {
            hold.started.notified().await;
            let rotated = service
                .update_model(
                    "admin-fixture".into(),
                    patch(Some("rotated-key"), Some(outcome == 2)),
                )
                .await;
            hold.release.notify_one();
            rotated.unwrap()
        };
        let (stale, rotated) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(check, rotation)
        })
        .await
        .expect("held request and public rotation must complete");
        assert_eq!(
            stale.err().expect("stale check must be rejected").0,
            StatusCode::CONFLICT
        );
        assert_eq!(rotated.is_active, outcome == 2);
        let current = service.get_model("admin-fixture".into()).await.unwrap();
        assert_eq!(
            current.is_active,
            outcome == 2,
            "old success or failure cannot change the replacement status"
        );
        assert!(current.thinking_probe.is_none());
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst) - before,
            2,
            "conflict must stop before probing thinking on the old configuration"
        );
        let checked = service.check_model("admin-fixture".into()).await.unwrap();
        assert!(checked.is_active);
        assert_eq!(checked.thinking_capability, Some(ThinkingCapability::Both));
    }
    // A later check of the same configuration owns its new observation;
    // an earlier in-flight check cannot overwrite that winning snapshot.
    hold.next.store(3, std::sync::atomic::Ordering::SeqCst);
    let older = service.check_model("admin-fixture".into());
    let newer = async {
        hold.started.notified().await;
        service.check_model("admin-fixture".into()).await.unwrap();
        let snapshot: String = sqlx::query_scalar(
            "SELECT CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
        ).fetch_one(pool.get()).await.unwrap();
        hold.release.notify_one();
        snapshot
    };
    let (stale, winning) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(older, newer)
    })
    .await
    .expect("concurrent checks must complete");
    assert_eq!(
        stale.err().expect("older observation must be fenced").0,
        StatusCode::CONFLICT
    );
    let stored: String = sqlx::query_scalar(
        "SELECT CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
    ).fetch_one(pool.get()).await.unwrap();
    assert_eq!(stored, winning);
    // A delayed PATCH must not partly overwrite a newer committed update or
    // clear the newer check's snapshot, regardless of its connectivity result.
    for (outcome, rotate_key) in [(1, true), (2, true), (1, false)] {
        hold.next
            .store(outcome, std::sync::atomic::Ordering::SeqCst);
        let mut old_patch = patch(Some("valid-key"), None);
        old_patch.tags = Some(vec!["stale".into()]);
        old_patch.description = Some("stale".into());
        let older = service.update_model("admin-fixture".into(), old_patch);
        let newer = async {
            hold.started.notified().await;
            let mut winner = patch(rotate_key.then_some("rotated-key"), Some(true));
            winner.tags = Some(vec!["winner".into()]);
            winner.description = Some("winner".into());
            winner.pricing = Some(astra_services::models::ConfiguredPricingData {
                currency: "USD".into(),
                unit: "per_token".into(),
                prompt: 0.000001,
                completion: 0.000002,
                cache_read: None,
                cache_write: None,
            });
            service
                .update_model("admin-fixture".into(), winner)
                .await
                .unwrap();
            let checked = service.check_model("admin-fixture".into()).await.unwrap();
            let snapshot: String = sqlx::query_scalar(
                "SELECT CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
            ).fetch_one(pool.get()).await.unwrap();
            hold.release.notify_one();
            (checked, snapshot)
        };
        let (stale, (winner, snapshot)) =
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(older, newer)
            })
            .await
            .expect("concurrent PATCH requests must finish");
        assert_eq!(
            stale.err().expect("old PATCH must conflict").0,
            StatusCode::CONFLICT
        );
        assert_eq!(winner.tags, ["winner"]);
        assert_eq!(winner.description.as_deref(), Some("winner"));
        assert_eq!(winner.pricing.prompt, 0.000001);
        assert_eq!(winner.pricing.completion, 0.000002);
        let current = service.get_model("admin-fixture".into()).await.unwrap();
        assert_eq!(current.model_id, winner.model_id);
        assert_eq!(current.tags, winner.tags);
        assert_eq!(current.description, winner.description);
        assert_eq!(current.pricing, winner.pricing);
        assert_eq!(current.quirks, winner.quirks);
        assert!(current.is_active);
        let (encrypted, stored): (String, String) = sqlx::query_as(
            "SELECT api_key_encrypted, CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
        ).fetch_one(pool.get()).await.unwrap();
        assert_eq!(encryptor.decrypt(&encrypted).unwrap(), "rotated-key");
        assert_eq!(stored, snapshot);
    }
    // Metadata and empty PATCHes preserve bound observations; empty arrays
    // replace their field instead of behaving as an absent value.
    let before = requests.load(std::sync::atomic::Ordering::SeqCst);
    let previous_snapshot: String = sqlx::query_scalar(
        "SELECT CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
    ).fetch_one(pool.get()).await.unwrap();
    let mut metadata = patch(None, None);
    metadata.tags = Some(vec![]);
    let changed = service
        .update_model("admin-fixture".into(), metadata)
        .await
        .unwrap();
    assert!(changed.tags.is_empty());
    assert_eq!(changed.thinking_capability, Some(ThinkingCapability::Both));
    let saved: (String, String) = sqlx::query_as(
        "SELECT CAST(updated_at AS CHAR), CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
    ).fetch_one(pool.get()).await.unwrap();
    assert_eq!(saved.1, previous_snapshot);
    service
        .update_model("admin-fixture".into(), patch(None, None))
        .await
        .unwrap();
    let restored: (String, String) = sqlx::query_as(
        "SELECT CAST(updated_at AS CHAR), CAST(thinking_probe_json AS CHAR) FROM infra_llm_models WHERE model_name = 'admin-fixture'",
    ).fetch_one(pool.get()).await.unwrap();
    assert_eq!(restored, saved);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), before);

    // Credential updates inherit a stored endpoint/header only when quirks
    // is absent. A replacement object must clear both of these old controls.
    for (configured, expected_error) in [
        (
            QuirksData {
                wire_model_name: Some("deepseek-chat".into()),
                probe_endpoint: Some("/missing-probe".into()),
                ..Default::default()
            },
            "HTTP 404",
        ),
        (
            QuirksData {
                wire_model_name: Some("deepseek-chat".into()),
                probe_headers: Some(
                    serde_json::from_value(json!({"x-provider-token":"invalid"})).unwrap(),
                ),
                ..Default::default()
            },
            "HTTP 401",
        ),
    ] {
        let mut stored = patch(None, None);
        stored.quirks = Some(configured);
        service
            .update_model("admin-fixture".into(), stored)
            .await
            .unwrap();
        let inherited = service
            .update_model("admin-fixture".into(), patch(Some("valid-key"), None))
            .await
            .unwrap();
        assert!(!inherited.is_active);
        assert!(
            inherited
                .connectivity
                .as_deref()
                .expect("stored probe failed")
                .starts_with(expected_error)
        );
        let mut reset = patch(Some("valid-key"), None);
        reset.quirks = request.quirks.clone();
        let cleared = service
            .update_model("admin-fixture".into(), reset)
            .await
            .unwrap();
        assert!(cleared.is_active);
        assert_eq!(cleared.connectivity.as_deref(), Some("ok"));
    }

    // A supplied quirks object is a full replacement. Removing its upstream
    // override makes the fixture reject the local alias, unlike the old merge.
    let mut replacement = patch(Some("valid-key"), None);
    replacement.quirks = Some(QuirksData::default());
    let disabled = service
        .update_model("admin-fixture".into(), replacement.clone())
        .await
        .unwrap();
    assert!(!disabled.is_active);
    assert!(
        disabled
            .connectivity
            .as_deref()
            .expect("failed probe result")
            .starts_with("HTTP 404")
    );
    assert!(disabled.quirks.wire_model_name.is_none());
    assert!(disabled.thinking_probe.is_none());
    replacement.is_active = Some(true);
    assert!(
        service
            .update_model("admin-fixture".into(), replacement)
            .await
            .unwrap()
            .is_active
    );
    let mut restore = patch(Some("valid-key"), None);
    restore.quirks = request.quirks.clone();
    assert!(
        service
            .update_model("admin-fixture".into(), restore)
            .await
            .unwrap()
            .is_active
    );

    // Name reuse is a new durable row, never the delayed PATCH's original target.
    let original = service.get_model("admin-fixture".into()).await.unwrap();
    hold.next.store(1, std::sync::atomic::Ordering::SeqCst);
    let older = service.update_model("admin-fixture".into(), patch(Some("rotated-key"), None));
    let recreate = async {
        hold.started.notified().await;
        service.delete_model("admin-fixture".into()).await.unwrap();
        let missing = service
            .delete_model("admin-fixture".into())
            .await
            .expect_err("second delete must miss");
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        let created = service
            .create_model(owner.clone(), request.clone())
            .await
            .unwrap();
        hold.release.notify_one();
        created
    };
    let (stale, recreated) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(older, recreate)
    })
    .await
    .expect("delete/recreate and delayed PATCH must finish");
    assert_eq!(
        stale.err().expect("replacement cannot receive old PATCH").0,
        StatusCode::CONFLICT
    );
    assert_ne!(recreated.model_id, original.model_id);
    let current = service.get_model("admin-fixture".into()).await.unwrap();
    assert_eq!(current.model_id, recreated.model_id);
    assert!(current.thinking_probe.is_none());
    let encrypted: String =
        sqlx::query_scalar("SELECT api_key_encrypted FROM infra_llm_models WHERE model_id = ?")
            .bind(&current.model_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(encryptor.decrypt(&encrypted).unwrap(), "valid-key");
    // A 404 from deletion also evicts a local Offering cached before another
    // Server removed the row; the known-missing target cannot survive via TTL.
    service
        .resolve_model_offering(current.model_id.clone())
        .await
        .unwrap();
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&current.model_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(matches!(
        service.delete_model("admin-fixture".into()).await,
        Err((StatusCode::NOT_FOUND, _))
    ));
    assert!(matches!(
        service.resolve_model_offering(current.model_id).await,
        Err((StatusCode::NOT_FOUND, _))
    ));

    service
        .delete_model("admin-verified-fixture".into())
        .await
        .unwrap();
    service
        .delete_user_model(owner, created.model_id)
        .await
        .unwrap();
    fixture.abort();
}
