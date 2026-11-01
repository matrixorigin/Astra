//! Non-billable OAuth/storage journey. Requires an explicitly isolated database.
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;
#[allow(dead_code)]
mod fixture {
    use astra_services as services;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/common/chatgpt_fixture.rs"
    ));
}
use astra_services::chatgpt::{
    ChatgptConfig, ChatgptConnections, CompleteAuthorization, ConnectionOperation,
    StartAuthorization,
};
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService,
    models::{UserModelCreateRequestData, UserModelUpdateRequestData},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
};

async fn operate_ok(
    service: &Arc<ChatgptConnections>,
    owner: &str,
    operation: ConnectionOperation,
) -> Value {
    service.operate(owner, operation).await.unwrap()
}

async fn start(service: &Arc<ChatgptConnections>, owner: &str, connection: Option<&str>) -> Value {
    operate_ok(
        service,
        owner,
        ConnectionOperation::Start(StartAuthorization {
            redirect_uri: "http://127.0.0.1:14555/auth/callback".into(),
            label: Some("Personal".into()),
            connection_id: connection.map(str::to_owned),
            retry_authorization_id: None,
        }),
    )
    .await
}
fn callback(value: &Value, code: &str) -> CompleteAuthorization {
    CompleteAuthorization {
        state: value["state"].as_str().unwrap().into(),
        code: Some(code.into()),
        client_id: Some("oaiapp_fixture".into()),
        error: None,
    }
}
async fn expire(pool: &sqlx::MySqlPool, key: &FernetTokenEncryptor, owner: &str, id: &str) {
    let encrypted:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(owner).bind(id).fetch_one(pool).await.unwrap();
    let mut value: Value = serde_json::from_str(&key.decrypt(&encrypted).unwrap()).unwrap();
    value["expires_at"] = json!(chrono::Utc::now().timestamp() - 1);
    sqlx::query("UPDATE user_llm_connections SET credentials_encrypted=?,token_expires_at=? WHERE user_id=? AND connection_id=?")
        .bind(key.encrypt(&value.to_string()).unwrap()).bind(chrono::Utc::now().timestamp()-1).bind(owner).bind(id).execute(pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn subscription_connection_ownership_refresh_models_and_disconnect() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    let (_, mut settings) = common::setup_pool_and_settings().await;
    // Reuse one connection across registration (NULL fields) and reauthorization
    // (populated fields) so cached nullable parameter metadata cannot hide a bug.
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = astra_core::SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let mut provider = fixture::ProviderFixture::start().await;
    let config = ChatgptConfig::for_test(&provider.base);
    let key = Arc::new(FernetTokenEncryptor::new("chatgpt-database-journey-test-key").unwrap());
    let service = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    let owner = format!("owner-{}", uuid::Uuid::new_v4());
    let other = format!("other-{}", uuid::Uuid::new_v4());
    let attempt = start(&service, &owner, None).await;
    let id = attempt["authorization_id"].as_str().unwrap();
    let code = provider.issue_code(&attempt, "account-one", true);
    let mut wrong = callback(&attempt, &code);
    wrong.state = "wrong-state".into();
    assert_eq!(
        service
            .operate(&owner, ConnectionOperation::Complete(id.into(), wrong))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::BAD_REQUEST
    );
    assert_eq!(provider.state.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .operate(&other, ConnectionOperation::Cancel(id.into()))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(
        service
            .operate(
                &other,
                ConnectionOperation::Complete(id.into(), callback(&attempt, &code))
            )
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    let linked = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Complete(id.into(), callback(&attempt, &code)),
    )
    .await;
    let connection = linked["connection"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(linked["connection"]["status"], "connected");
    let repeated = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Complete(id.into(), callback(&attempt, &code)),
    )
    .await;
    assert_eq!(linked, repeated);
    assert_eq!(provider.state.token_calls.load(Ordering::SeqCst), 1);
    let stored:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    assert!(!stored.contains("fixture-access-"));
    let plaintext = key.decrypt(&stored).unwrap();
    assert!(plaintext.contains("fixture-access-"));
    assert!(
        serde_json::from_str::<Value>(&plaintext)
            .unwrap()
            .get("id_token")
            .is_none()
    );
    let metadata = operate_ok(&service, &owner, ConnectionOperation::List)
        .await
        .to_string();
    assert!(
        !metadata.contains("fixture-access-")
            && !metadata.contains("fixture-refresh-")
            && !metadata.contains("id_token")
    );
    assert_eq!(
        service
            .operate(&other, ConnectionOperation::Models(connection.clone()))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let models = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(shared.clone())
        .with_chatgpt_config(config.clone());
    let mut model_ids = Vec::new();
    for (index, slug) in ["fixture-model", "fixture-second"].into_iter().enumerate() {
        let model = models
            .create_user_model(
                owner.clone(),
                UserModelCreateRequestData {
                    name: format!("subscription-{index}"),
                    provider: "chatgpt".into(),
                    model: slug.into(),
                    base_url: None,
                    api_key: String::new(),
                    connection_id: Some(connection.clone()),
                    context_window: 128000,
                    is_default: index == 0,
                },
            )
            .await
            .unwrap();
        model_ids.push(model.model_id);
    }
    let configured = service
        .operate(&owner, ConnectionOperation::Models(connection.clone()))
        .await
        .unwrap();
    let configured_ids: HashSet<_> = configured["configured_models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["model_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(configured_ids, model_ids.iter().cloned().collect());
    let catalog = models.list_models(owner.clone(), false).await.unwrap();
    let own: Vec<_> = catalog
        .iter()
        .filter(|item| model_ids.contains(&item.offering_id))
        .collect();
    assert_eq!(own.len(), 2);
    let first_item = own
        .iter()
        .find(|item| item.offering_id == model_ids[0])
        .unwrap();
    assert_eq!(
        first_item.thinking_protocol,
        Some(astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning)
    );
    assert_eq!(
        first_item.thinking_capability,
        Some(astra_services::models::ThinkingCapability::EffortOnly)
    );
    assert_eq!(
        first_item.supported_reasoning_efforts.as_ref().unwrap(),
        &vec![
            "low".to_string(),
            "medium".into(),
            "high".into(),
            "xhigh".into(),
            "max".into()
        ],
        "the protocol inventory excludes upstream-only controls such as ultra"
    );
    let second_execution = models
        .admit_model_offering(owner.clone(), model_ids[1].clone())
        .await
        .unwrap();
    assert_eq!(
        second_execution.supported_reasoning_efforts,
        Some(vec!["high".into()])
    );

    assert!(own.iter().all(|item| item.access_id==format!("chatgpt:{connection}") && item.pricing.is_none()));
    assert!(
        models
            .list_models(other.clone(), false)
            .await
            .unwrap()
            .iter()
            .all(|item| !model_ids.contains(&item.offering_id))
    );
    let first = models
        .admit_model_offering(owner.clone(), model_ids[0].clone())
        .await
        .unwrap()
        .subscription
        .unwrap();
    let second = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(shared.clone())
        .with_chatgpt_config(config.clone())
        .admit_model_offering(owner.clone(), model_ids[1].clone())
        .await
        .unwrap()
        .subscription
        .unwrap();
    // Every leg fits its 15-second timeout, but the old token + discovery +
    // JWKS chain exceeded the 30-second rotation lease. Metadata is prepared
    // before claiming that lease and reused while verifying the response.
    provider
        .state
        .metadata_delay_ms
        .store(11000, Ordering::SeqCst);
    provider
        .state
        .refresh_delay_ms
        .store(11000, Ordering::SeqCst);
    expire(&pool, &key, &owner, &connection).await;
    let (a, b) = tokio::join!(first.acquire(None), second.acquire(None));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.bearer(), b.bearer());
    provider.state.metadata_delay_ms.store(0, Ordering::SeqCst);
    provider.state.refresh_delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(provider.state.refresh_calls.load(Ordering::SeqCst), 1);
    drop((a, b));
    let primary = service.binding(&owner, &connection).await.unwrap();
    expire(&pool, &key, &owner, &connection).await;
    let previous:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    provider.stop().await;
    assert_eq!(
        primary.acquire(None).await.err().unwrap().0,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let after:(String,String,Option<String>)=sqlx::query_as("SELECT status,credentials_encrypted,refresh_owner FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    assert_eq!(
        after,
        ("connected".into(), previous, None),
        "an undispatched refresh must retain its credential and release its lease"
    );
    assert!(
        models
            .list_models(owner.clone(), false)
            .await
            .unwrap()
            .iter()
            .any(|item| item.offering_id == model_ids[0])
    );
    provider.restart().await;
    drop(primary.acquire(None).await.unwrap());
    assert_eq!(provider.state.refresh_calls.load(Ordering::SeqCst), 2);

    // Even the last waiter can disappear after dispatch. The connection owner
    // must still persist the rotating token instead of stranding its lease.
    let version = primary.acquire(None).await.unwrap().version();
    expire(&pool, &key, &owner, &connection).await;
    provider.state.refresh_delay_ms.store(200, Ordering::SeqCst);
    let cancelled = tokio::spawn({
        let primary = primary.clone();
        async move { primary.acquire(None).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while provider.state.refresh_calls.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancelled.abort();
    assert!(cancelled.await.err().unwrap().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let state:(String,i64,Option<String>)=sqlx::query_as("SELECT status,credential_version,refresh_owner FROM user_llm_connections WHERE user_id=? AND connection_id=?")
                .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
            assert_eq!(state.0, "connected");
            if state.1 == version + 1 && state.2.is_none() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let refreshed = primary.acquire(Some(version)).await.unwrap();
    assert_eq!(refreshed.version(), version + 1);
    drop(refreshed);
    assert_eq!(provider.state.refresh_calls.load(Ordering::SeqCst), 3);

    // A simultaneous expiry shares one worker/result; valid acquisitions reuse it.
    expire(&pool, &key, &owner, &connection).await;
    for expected_refreshes in [4, 4] {
        let leases = futures_util::future::join_all((0..64).map(|_| primary.acquire(None))).await;
        let leases = leases.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(
            leases
                .iter()
                .all(|lease| lease.bearer() == leases[0].bearer())
        );
        assert_eq!(
            provider.state.refresh_calls.load(Ordering::SeqCst),
            expected_refreshes
        );
    }
    provider.state.refresh_delay_ms.store(0, Ordering::SeqCst);
    let live = first.acquire(None).await.unwrap();
    expire(&pool, &key, &owner, &connection).await;
    provider.state.refresh_delay_ms.store(200, Ordering::SeqCst);
    let interrupted_refresh = tokio::spawn({
        let first = first.clone();
        async move { first.acquire(None).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while provider.state.refresh_calls.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let disconnected = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Disconnect(connection.clone()),
    )
    .await;
    assert_eq!(disconnected["status"], "disconnected");
    assert_eq!(disconnected["remote_revocation_confirmed"], true);
    assert_eq!(
        operate_ok(&service, &owner, ConnectionOperation::List).await["items"][0]["remote_revocation_confirmed"],
        true
    );
    assert!(
        interrupted_refresh.await.unwrap().is_err(),
        "an owned refresh must not overwrite disconnect"
    );
    provider.state.refresh_delay_ms.store(0, Ordering::SeqCst);
    assert!(first.ensure_current().await.is_err());
    // This binding belongs to another service instance: database fencing is
    // authoritative even when local cancellation registries are different.
    assert!(second.ensure_current().await.is_err());
    drop(live);
    assert_eq!(
        models
            .default_user_model_offering_id(owner.clone())
            .await
            .unwrap(),
        Some(model_ids[0].clone())
    );
    assert!(
        models
            .default_chat_model_offering_id(owner.clone())
            .await
            .is_err(),
        "a disconnected subscription default must not choose a deployment model"
    );
    assert!(
        models
            .admit_model_offering(owner.clone(), model_ids[0].clone())
            .await
            .is_err()
    );

    // A reauthorization belongs to the existing verified identity, not email.
    let relink = start(&service, &owner, Some(&connection)).await;
    let fence:(Option<String>,Option<String>)=sqlx::query_as("SELECT connection_id,expected_subject FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
        .bind(&owner).bind(relink["authorization_id"].as_str().unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        fence,
        (Some(connection.clone()), Some("account-one".into()))
    );
    let wrong_code = provider.issue_code(&relink, "another-account", true);
    assert!(
        service
            .operate(
                &owner,
                ConnectionOperation::Complete(
                    relink["authorization_id"].as_str().unwrap().into(),
                    callback(&relink, &wrong_code)
                )
            )
            .await
            .is_err()
    );
    let good = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Start(StartAuthorization {
            redirect_uri: "http://127.0.0.1:14555/auth/callback".into(),
            label: None,
            connection_id: None,
            retry_authorization_id: Some(relink["authorization_id"].as_str().unwrap().into()),
        }),
    )
    .await;
    let retry_url = reqwest::Url::parse(good["authorization_url"].as_str().unwrap()).unwrap();
    assert!(
        retry_url
            .query_pairs()
            .any(|(key, value)| key == "client_id" && value == "oaiapp_fixture")
    );
    assert!(
        !retry_url
            .query_pairs()
            .any(|(key, _)| key == "agent_name_hint")
    );
    let good_code = provider.issue_code(&good, "account-one", true);
    operate_ok(
        &service,
        &owner,
        ConnectionOperation::Complete(
            good["authorization_id"].as_str().unwrap().into(),
            callback(&good, &good_code),
        ),
    )
    .await;
    let renewed = service.binding(&owner, &connection).await.unwrap();
    assert!(
        first.ensure_current().await.is_err(),
        "old generation must remain fenced"
    );
    let bytes = renewed.acquire(None).await.unwrap();
    assert!(!bytes.bearer().is_empty());
    provider.reject_access_token(bytes.bearer(), false);
    drop(bytes);
    let refreshes = provider.state.refresh_calls.load(Ordering::SeqCst);
    service
        .probe(&owner, &connection, "fixture-model")
        .await
        .unwrap();
    assert_eq!(
        provider.state.refresh_calls.load(Ordering::SeqCst),
        refreshes + 1
    );
    provider.state.stream_quota.store(true, Ordering::SeqCst);
    let failed = service
        .probe(&owner, &connection, "fixture-model")
        .await
        .unwrap_err();
    provider.state.stream_quota.store(false, Ordering::SeqCst);
    assert_eq!(failed.0, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        failed.1.0.error_code.as_deref(),
        Some("subscription_sharing_usage_limit_exceeded")
    );
    provider.state.truncate.store(true, Ordering::SeqCst);
    let failed = service
        .probe(&owner, &connection, "fixture-model")
        .await
        .unwrap_err();
    provider.state.truncate.store(false, Ordering::SeqCst);
    assert_eq!(failed.0, axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        failed.1.0.error_code.as_deref(),
        Some("chatgpt_model_check_failed")
    );
    *provider.state.reject_token.lock().unwrap() = None;
    expire(&pool, &key, &owner, &connection).await;
    let before:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    provider.state.refresh_error.store(429, Ordering::SeqCst);
    assert_eq!(
        renewed.acquire(None).await.err().unwrap().0,
        axum::http::StatusCode::TOO_MANY_REQUESTS
    );
    let after:(String,String,Option<String>)=sqlx::query_as("SELECT status,credentials_encrypted,refresh_owner FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    assert_eq!(after, ("connected".into(), before, None));
    provider.state.refresh_error.store(400, Ordering::SeqCst);
    assert!(renewed.acquire(None).await.is_err());
    assert_eq!(
        operate_ok(&service, &owner, ConnectionOperation::List).await["items"][0]["status"],
        "reauth_required"
    );

    let no_plan = start(&service, &other, None).await;
    let no_plan_code = provider.issue_code(&no_plan, "account-two", false);
    let disabled = operate_ok(
        &service,
        &other,
        ConnectionOperation::Complete(
            no_plan["authorization_id"].as_str().unwrap().into(),
            callback(&no_plan, &no_plan_code),
        ),
    )
    .await;
    let disabled_id = disabled["connection"]["connection_id"].as_str().unwrap();
    assert_eq!(disabled["connection"]["status"], "plan_disabled");
    assert_eq!(
        service.binding(&other, disabled_id).await.unwrap_err().0,
        axum::http::StatusCode::FORBIDDEN
    );
    let consent = start(&service, &other, Some(disabled_id)).await;
    let url = reqwest::Url::parse(consent["authorization_url"].as_str().unwrap()).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "prompt" && value == "consent")
    );
    assert!(!url.query_pairs().any(|(key, _)| key == "agent_name_hint"));
    operate_ok(
        &service,
        &other,
        ConnectionOperation::Cancel(consent["authorization_id"].as_str().unwrap().into()),
    )
    .await;
    let codes: HashSet<_> = provider
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["model"].as_str().unwrap().to_owned())
        .collect();
    assert!(codes.contains("fixture-model") && codes.contains("fixture-second"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn disconnect_fences_inflight_model_creation_check_activation_and_default() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let provider = fixture::ProviderFixture::start().await;
    let config = ChatgptConfig::for_test(&provider.base);
    let key = Arc::new(FernetTokenEncryptor::new("subscription-activation-race-test").unwrap());
    let connections = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    let models = Arc::new(
        DatabaseModelService::new(settings, key)
            .with_pool(shared.clone())
            .with_chatgpt_config(config),
    );
    for operation in ["check", "activate", "default", "create"] {
        let owner = format!("activation-{}", uuid::Uuid::new_v4());
        let connection = provider.connect(models.as_ref(), &owner).await;
        let model_request = UserModelCreateRequestData {
            connection_id: Some(connection.clone()),
            name: "initial".into(),
            provider: "chatgpt".into(),
            model: "fixture-model".into(),
            base_url: None,
            api_key: String::new(),
            context_window: 128000,
            is_default: true,
        };
        let model = models
            .create_user_model(owner.clone(), model_request.clone())
            .await
            .unwrap();
        let mut blocked = shared.get().begin().await.unwrap();
        sqlx::query(
            "SELECT model_id FROM user_llm_models WHERE user_id=? AND model_id=? FOR UPDATE",
        )
        .bind(&owner)
        .bind(&model.model_id)
        .fetch_one(&mut *blocked)
        .await
        .unwrap();
        let disconnect = tokio::spawn({
            let connections = connections.clone();
            let owner = owner.clone();
            let connection = connection.clone();
            async move {
                connections
                    .operate(&owner, ConnectionOperation::Disconnect(connection))
                    .await
            }
        });
        // Disconnect first locks the connection, then waits for the model row.
        // Probes can still see the old committed connection during that wait.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let before = provider.state.responses_calls.load(Ordering::SeqCst);
        let activation = tokio::spawn({
            let models = models.clone();
            let owner = owner.clone();
            let model_id = model.model_id.clone();
            async move {
                match operation {
                    "check" => models.check_user_model(owner, model_id).await,
                    "create" => {
                        models
                            .create_user_model(
                                owner,
                                UserModelCreateRequestData {
                                    name: "new-model".into(),
                                    ..model_request
                                },
                            )
                            .await
                    }
                    _ => {
                        models
                            .update_user_model(
                                owner,
                                model_id,
                                UserModelUpdateRequestData {
                                    is_active: (operation == "activate").then_some(true),
                                    is_default: (operation == "default").then_some(true),
                                    ..Default::default()
                                },
                            )
                            .await
                    }
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while provider.state.responses_calls.load(Ordering::SeqCst) == before {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        blocked.commit().await.unwrap();
        disconnect.await.unwrap().unwrap();
        assert!(
            activation.await.unwrap().is_err(),
            "stale {operation} succeeded after disconnect"
        );
        let state:(String,i64)=sqlx::query_as("SELECT c.status, COUNT(CASE WHEN m.is_active=1 THEN 1 END) FROM user_llm_connections c LEFT JOIN user_llm_models m ON c.user_id=m.user_id AND c.connection_id=m.connection_id WHERE c.user_id=? AND c.connection_id=? GROUP BY c.status")
            .bind(&owner).bind(&connection).fetch_one(shared.get()).await.unwrap();
        assert_eq!(state, ("disconnected".into(), 0), "{operation}");
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn reauthorization_completion_uses_disconnect_lock_order() {
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let worker = astra_core::SharedPool::new(&settings).await.unwrap();
    let worker_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(worker.get())
        .await
        .unwrap();
    let provider = fixture::ProviderFixture::start().await;
    let service = Arc::new(ChatgptConnections::new(
        Some(worker),
        Arc::new(FernetTokenEncryptor::new("subscription-reauthorization-race").unwrap()),
        Ok(ChatgptConfig::for_test(&provider.base)),
    ));
    let owner = format!("reauthorization-{}", uuid::Uuid::new_v4());
    let initial = start(&service, &owner, None).await;
    let code = provider.issue_code(&initial, "same-account", true);
    let linked = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Complete(
            initial["authorization_id"].as_str().unwrap().into(),
            callback(&initial, &code),
        ),
    )
    .await;
    let connection = linked["connection"]["connection_id"].as_str().unwrap();
    let attempt = start(&service, &owner, Some(connection)).await;
    let authorization = attempt["authorization_id"].as_str().unwrap();
    let code = provider.issue_code(&attempt, "same-account", true);
    let exchanges = provider.state.token_calls.load(Ordering::SeqCst);

    // Hold the first row owned by disconnect, then let real completion exchange
    // its code and reach the blocked connection write/read. Observing its SQL
    // makes this interleaving independent of HTTP and scheduler timing.
    let mut disconnect = shared.get().begin().await.unwrap();
    sqlx::query("SELECT connection_id FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
        .bind(&owner).bind(connection).fetch_one(&mut *disconnect).await.unwrap();
    let completion = tokio::spawn({
        let service = service.clone();
        let owner = owner.clone();
        let operation =
            ConnectionOperation::Complete(authorization.into(), callback(&attempt, &code));
        async move { service.operate(&owner, operation).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sql: Option<String> = sqlx::query_scalar(
                "SELECT info FROM information_schema.processlist WHERE db=? AND conn_id=? AND info IS NOT NULL",
            ).bind(&settings.database).bind(worker_id).fetch_optional(shared.get()).await.unwrap().flatten();
            if sql.is_some_and(|sql| sql.contains("user_llm_connections")
                && (sql.contains("FOR UPDATE") || sql.contains("INSERT INTO"))) { break; }
            assert!(!completion.is_finished(), "completion settled before reaching the connection lock");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("completion must wait on disconnect's connection row");
    assert_eq!(
        provider.state.token_calls.load(Ordering::SeqCst),
        exchanges + 1
    );

    // These are disconnect's fenced writes. Completion must not already own
    // the authorization row while waiting for this connection row.
    sqlx::query("UPDATE user_llm_connections SET status='disconnected',credentials_encrypted=NULL,token_expires_at=NULL,generation=generation+1 WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(connection).execute(&mut *disconnect).await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(5),
        sqlx::query("UPDATE user_llm_authorizations SET status='cancelled',verifier_encrypted=NULL WHERE user_id=? AND connection_id=? AND status IN ('pending','exchanging')")
            .bind(&owner).bind(connection).execute(&mut *disconnect),
    ).await.expect("authorization cancellation must not deadlock with completion").unwrap();
    assert_eq!(cancelled.rows_affected(), 1);
    disconnect.commit().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.0, axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.1.0.error_code.as_deref(),
        Some("oauth_attempt_cancelled")
    );
    assert_eq!(
        provider.state.token_calls.load(Ordering::SeqCst),
        exchanges + 1
    );
    let persisted: (String, Option<String>) = sqlx::query_as(
        "SELECT status,credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?",
    ).bind(&owner).bind(connection).fetch_one(shared.get()).await.unwrap();
    assert_eq!(persisted, ("disconnected".into(), None));
    let status: String = sqlx::query_scalar(
        "SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?",
    )
    .bind(&owner)
    .bind(authorization)
    .fetch_one(shared.get())
    .await
    .unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn legacy_byok_rows_survive_idempotent_connection_migration() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    let (shared, mut settings) = common::setup_pool_and_settings().await;
    let database = format!("sub_auth_migration_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(shared.get())
        .await
        .unwrap();
    settings.database = database.clone();
    let migration_pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let pool = migration_pool.get();
    sqlx::query("CREATE TABLE user_llm_models (model_id VARCHAR(64) PRIMARY KEY, api_key_encrypted TEXT NOT NULL)").execute(pool).await.unwrap();
    let key = FernetTokenEncryptor::new("legacy-migration-test-key").unwrap();
    let ciphertext = key.encrypt("legacy-fake-byok-key").unwrap();
    sqlx::query("INSERT INTO user_llm_models (model_id,api_key_encrypted) VALUES ('legacy',?)")
        .bind(&ciphertext)
        .execute(pool)
        .await
        .unwrap();
    astra_services::chatgpt::bootstrap(pool).await.unwrap();
    astra_services::chatgpt::bootstrap(pool).await.unwrap();
    let stored: (String, Option<String>) = sqlx::query_as(
        "SELECT api_key_encrypted,connection_id FROM user_llm_models WHERE model_id='legacy'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(stored, (ciphertext, None));
    sqlx::query("INSERT INTO user_llm_models (model_id,api_key_encrypted,connection_id) VALUES ('subscription',NULL,'fixture-connection')").execute(pool).await.unwrap();
    migration_pool.get().close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(shared.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn repeated_disconnect_cancels_pending_and_exchanging_reauthorization() {
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let shared = common::setup_pool().await;
    let provider = fixture::ProviderFixture::start().await;
    let service = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        Arc::new(FernetTokenEncryptor::new("repeated-disconnect-test").unwrap()),
        Ok(ChatgptConfig::for_test(&provider.base)),
    ));
    let owner = format!("disconnect-{}", uuid::Uuid::new_v4());
    let initial = start(&service, &owner, None).await;
    let code = provider.issue_code(&initial, "same-account", true);
    let linked = operate_ok(
        &service,
        &owner,
        ConnectionOperation::Complete(
            initial["authorization_id"].as_str().unwrap().into(),
            callback(&initial, &code),
        ),
    )
    .await;
    let id = linked["connection"]["connection_id"].as_str().unwrap();
    operate_ok(&service, &owner, ConnectionOperation::Disconnect(id.into())).await;
    for exchanging in [false, true] {
        let attempt = start(&service, &owner, Some(id)).await;
        let authorization = attempt["authorization_id"].as_str().unwrap();
        let code = provider.issue_code(&attempt, "same-account", true);
        provider
            .state
            .hold_code_exchange
            .store(exchanging, Ordering::SeqCst);
        let mut operation = Some(ConnectionOperation::Complete(
            authorization.into(),
            callback(&attempt, &code),
        ));
        let exchanges = provider.state.token_calls.load(Ordering::SeqCst);
        let completion = if exchanging {
            let service = service.clone();
            let owner = owner.clone();
            let operation = operation.take().unwrap();
            let task = tokio::spawn(async move { service.operate(&owner, operation).await });
            tokio::time::timeout(
                Duration::from_secs(5),
                provider.state.code_exchange_started.notified(),
            )
            .await
            .expect("completion must reach the provider exchange");
            Some(task)
        } else {
            None
        };
        if exchanging {
            let status: String = sqlx::query_scalar(
                "SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?",
            )
            .bind(&owner)
            .bind(authorization)
            .fetch_one(shared.get())
            .await
            .unwrap();
            assert_eq!(status, "exchanging");
        }
        operate_ok(&service, &owner, ConnectionOperation::Disconnect(id.into())).await;
        if exchanging {
            provider.state.release_code_exchange.notify_one();
        }
        let result = match completion {
            Some(task) => tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            None => service.operate(&owner, operation.take().unwrap()).await,
        };
        let error = result.expect_err("disconnect must fence late authorization completion");
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some(if exchanging {
                "oauth_attempt_cancelled"
            } else {
                "oauth_attempt_expired"
            })
        );
        assert_eq!(
            provider.state.token_calls.load(Ordering::SeqCst),
            exchanges + usize::from(exchanging)
        );
        let persisted: (String, Option<String>) = sqlx::query_as(
            "SELECT status,credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
            .bind(&owner).bind(id).fetch_one(shared.get()).await.unwrap();
        assert_eq!(persisted, ("disconnected".into(), None));
        let cancelled: (String, Option<String>) = sqlx::query_as(
            "SELECT status,verifier_encrypted FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
            .bind(&owner).bind(authorization).fetch_one(shared.get()).await.unwrap();
        assert_eq!(cancelled, ("cancelled".into(), None));
    }
}

#[allow(dead_code)]
#[path = "common/mysql_response_proxy.rs"]
mod mysql_response_proxy;

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn subscription_sql_timeouts_release_pool_capacity_before_response_arrives() {
    use mysql_response_proxy::{Boundary, ResponseProxy};
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let provider = fixture::ProviderFixture::start().await;
    let key = Arc::new(FernetTokenEncryptor::new("subscription-timeout-pool").unwrap());
    let config = ChatgptConfig::for_test(&provider.base);
    let direct = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    for refresh in [false, true] {
        let owner = format!("sql-timeout-{}", uuid::Uuid::new_v4());
        let initial = start(&direct, &owner, None).await;
        let code = provider.issue_code(&initial, "same-account", true);
        let linked = operate_ok(
            &direct,
            &owner,
            ConnectionOperation::Complete(
                initial["authorization_id"].as_str().unwrap().into(),
                callback(&initial, &code),
            ),
        )
        .await;
        let id = linked["connection"]["connection_id"].as_str().unwrap();
        let prefix: &'static [u8] = if refresh {
            b"UPDATE user_llm_connections SET credentials_encrypted="
        } else {
            b"SELECT status,generation FROM user_llm_connections"
        };
        let proxy = ResponseProxy::start(
            settings.host.clone(),
            settings.port,
            Boundary::Statement(prefix),
        )
        .await;
        let worker = proxy_pool(&settings, proxy.port).await;
        let service = Arc::new(ChatgptConnections::new(
            Some(worker.clone()),
            key.clone(),
            Ok(config.clone()),
        ));
        let binding = service.binding(&owner, id).await.unwrap();
        if refresh {
            expire(shared.get(), &key, &owner, id).await;
        }
        let mut acquire = Some(tokio::spawn(async move { binding.acquire(None).await }));
        let lease = if refresh {
            None
        } else {
            Some(acquire.take().unwrap().await.unwrap().unwrap())
        };
        tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
            .await
            .expect("the real database must execute SQL before its response is withheld");
        if let Some(lease) = &lease {
            tokio::time::timeout(Duration::from_secs(4), lease.cancel.cancelled())
                .await
                .expect("watcher timeout must cancel inference");
        }
        // Do not release the response: a pooled SQLx checkout would keep its
        // sole permit while ping drains that reply. The guard must discard it.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if worker.get().num_idle() > 0 || worker.get().size() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(worker.get())
                .await
                .unwrap()
        })
        .await
        .expect("pool capacity must recover before releasing the old response");
        if let Some(acquire) = acquire {
            let recovered = tokio::time::timeout(Duration::from_secs(3), acquire)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                recovered.version(),
                2,
                "reuse the committed rotation after losing its SQL acknowledgement"
            );
            assert!(!recovered.cancel.is_cancelled());
        }
        proxy.gate.release.notify_one();
        drop(lease);
        worker.get().close().await;
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn personal_management_is_available_before_models_exist_and_keeps_uc_managed() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let issuer = "http://127.0.0.1:9/realms/moi";
    let provider = astra_services::auth::uc::UcNativeProvider::new(astra_core::UcNativeSettings {
        builtin_memory: false,
        issuer: issuer.into(),
        adapter_url: "http://127.0.0.1:9".into(),
        moi_api_url: "http://127.0.0.1:9/newmoi".into(),
        genesis_url: "http://127.0.0.1:9".into(),
        client_secret: "synthetic-service-secret".into(),
    })
    .unwrap();
    let service = DatabaseModelService::new(
        settings,
        Arc::new(FernetTokenEncryptor::new("management-policy-test").unwrap()),
    )
    .with_pool(shared.clone())
    .with_uc_native(Some(provider));
    let local = format!("local-{}", uuid::Uuid::new_v4());
    let uc = format!("uc-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?,?,?)")
        .bind(format!("uc:{issuer}")).bind(&uc).bind(&uc).execute(shared.get()).await.unwrap();
    let empty = service
        .model_connection_operation(local.clone(), ConnectionOperation::List)
        .await
        .unwrap();
    assert_eq!(empty["management_available"], true);
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["chatgpt"]["available"], true);
    let managed = service
        .model_connection_operation(uc.clone(), ConnectionOperation::List)
        .await
        .unwrap();
    assert_eq!(managed["management_available"], false);
    assert_eq!(managed["chatgpt"]["available"], false);
    assert_eq!(managed["items"], json!([]));
    let denied = service
        .model_connection_operation(
            uc,
            ConnectionOperation::Start(StartAuthorization {
                retry_authorization_id: None,
                redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
                label: None,
                connection_id: None,
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.0, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        denied.1.0.error_code.as_deref(),
        Some("personal_model_management_unavailable")
    );
    let mut config = astra_services::chatgpt::ChatgptConfig::for_test("http://127.0.0.1:9");
    config.enabled = false;
    let disabled = service
        .with_chatgpt_config(config)
        .model_connection_operation(local, ConnectionOperation::List)
        .await
        .unwrap();
    assert_eq!(
        disabled["management_available"], true,
        "disabling ChatGPT must retain API-key management"
    );
    assert_eq!(
        disabled["chatgpt"]["error_code"],
        "chatgpt_subscription_disabled"
    );
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn simultaneous_registration_retries_preserve_canonical_connection() {
    use mysql_response_proxy::{Boundary, ResponseProxy};
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let provider = fixture::ProviderFixture::start().await;
    let key = Arc::new(FernetTokenEncryptor::new("review-concurrent-link").unwrap());
    let config = ChatgptConfig::for_test(&provider.base);
    let direct = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::Statement(b"SELECT connection_id FROM user_llm_connections WHERE user_id="),
    )
    .await;
    let worker = proxy_pool(&settings, proxy.port).await;
    let delayed = Arc::new(ChatgptConnections::new(Some(worker), key, Ok(config)));
    let owner = format!("review-{}", uuid::Uuid::new_v4());
    // A failed first link retains the issued client but has no connection yet.
    // Two real retry operations reuse that issued registration, as required by OAuth.
    let failed = start(&direct, &owner, None).await;
    assert!(
        direct
            .operate(
                &owner,
                ConnectionOperation::Complete(
                    failed["authorization_id"].as_str().unwrap().into(),
                    callback(&failed, "unknown-code")
                )
            )
            .await
            .is_err()
    );
    let first = operate_ok(
        &direct,
        &owner,
        ConnectionOperation::Start(StartAuthorization {
            retry_authorization_id: Some(failed["authorization_id"].as_str().unwrap().into()),
            redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
            label: None,
            connection_id: None,
        }),
    )
    .await;
    let second = operate_ok(
        &direct,
        &owner,
        ConnectionOperation::Start(StartAuthorization {
            retry_authorization_id: Some(failed["authorization_id"].as_str().unwrap().into()),
            redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
            label: None,
            connection_id: None,
        }),
    )
    .await;
    let first_code = provider.issue_code(&first, "same-subject", true);
    let second_code = provider.issue_code(&second, "same-subject", true);
    let task = tokio::spawn({
        let delayed = delayed.clone();
        let owner = owner.clone();
        let first = first.clone();
        let first_code = first_code.clone();
        async move {
            delayed
                .operate(
                    &owner,
                    ConnectionOperation::Complete(
                        first["authorization_id"].as_str().unwrap().into(),
                        callback(&first, &first_code),
                    ),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
        .await
        .unwrap();
    let winning = operate_ok(
        &direct,
        &owner,
        ConnectionOperation::Complete(
            second["authorization_id"].as_str().unwrap().into(),
            callback(&second, &second_code),
        ),
    )
    .await;
    proxy.gate.release.notify_one();
    let first_result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    let record: (String, Option<String>) = sqlx::query_as("SELECT status, completed_connection_id FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
        .bind(&owner).bind(first["authorization_id"].as_str().unwrap()).fetch_one(shared.get()).await.unwrap();
    println!(
        "delayed result status={:?}, authorization={:?}, winner={}",
        first_result
            .as_ref()
            .map(|_| 200)
            .unwrap_or_else(|e| e.0.as_u16()),
        record,
        winning["connection"]["connection_id"]
    );
    assert!(
        first_result.is_ok(),
        "both valid authorizations must resolve to the actual stored connection"
    );
    assert_eq!(
        first_result.unwrap()["connection"]["connection_id"],
        winning["connection"]["connection_id"]
    );
    let completed: Vec<String> = sqlx::query_scalar("SELECT completed_connection_id FROM user_llm_authorizations WHERE user_id=? AND status='completed'")
        .bind(&owner).fetch_all(shared.get()).await.unwrap();
    assert_eq!(completed.len(), 2);
    assert!(
        completed
            .iter()
            .all(|id| id == winning["connection"]["connection_id"].as_str().unwrap())
    );
    let replay = operate_ok(
        &direct,
        &owner,
        ConnectionOperation::Complete(
            first["authorization_id"].as_str().unwrap().into(),
            callback(&first, &first_code),
        ),
    )
    .await;
    assert_eq!(
        replay["connection"]["connection_id"],
        winning["connection"]["connection_id"]
    );
    assert_eq!(
        provider
            .state
            .token_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        3
    );
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn completion_settles_after_waiter_cancellation_and_rolls_back_cancelled_link() {
    use mysql_response_proxy::{Boundary, ResponseProxy};
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let provider = fixture::ProviderFixture::start().await;
    let key = Arc::new(FernetTokenEncryptor::new("cancelled-completion").unwrap());
    let config = ChatgptConfig::for_test(&provider.base);
    let direct = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    let proxy = ResponseProxy::start(
        settings.host.clone(),
        settings.port,
        Boundary::Statement(b"INSERT INTO user_llm_connections "),
    )
    .await;
    let worker = proxy_pool(&settings, proxy.port).await;
    let delayed = Arc::new(ChatgptConnections::new(
        Some(worker.clone()),
        key,
        Ok(config),
    ));
    let owner = format!("cancel-complete-{}", uuid::Uuid::new_v4());
    let auth = start(&direct, &owner, None).await;
    let code = provider.issue_code(&auth, "cancelled-subject", true);
    let waiter = tokio::spawn({
        let owner = owner.clone();
        let auth = auth.clone();
        async move {
            delayed
                .operate(
                    &owner,
                    ConnectionOperation::Complete(
                        auth["authorization_id"].as_str().unwrap().into(),
                        callback(&auth, &code),
                    ),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    operate_ok(
        &direct,
        &owner,
        ConnectionOperation::Cancel(auth["authorization_id"].as_str().unwrap().into()),
    )
    .await;
    proxy.gate.release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(worker.get()),
    )
    .await
    .unwrap()
    .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_llm_connections WHERE user_id=?")
        .bind(&owner)
        .fetch_one(shared.get())
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "cancelled authorization must roll back its provisional connection"
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?",
    )
    .bind(&owner)
    .bind(auth["authorization_id"].as_str().unwrap())
    .fetch_one(shared.get())
    .await
    .unwrap();
    assert_eq!(status, "cancelled");
    assert_eq!(
        provider
            .state
            .token_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    worker.get().close().await;
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn completion_deadline_releases_sql_and_only_settles_its_own_exchange() {
    use mysql_response_proxy::{Boundary, ResponseProxy};
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    for before_claim in [true, false] {
        let provider = fixture::ProviderFixture::start().await;
        let key = Arc::new(FernetTokenEncryptor::new("deadline-completion").unwrap());
        let config = ChatgptConfig::for_test(&provider.base);
        let direct = Arc::new(ChatgptConnections::new(
            Some(shared.clone()),
            key.clone(),
            Ok(config.clone()),
        ));
        let prefix: &'static [u8] = if before_claim {
            b"SELECT * FROM user_llm_authorizations"
        } else {
            b"SELECT connection_id FROM user_llm_connections WHERE user_id="
        };
        let proxy = ResponseProxy::start(
            settings.host.clone(),
            settings.port,
            Boundary::Statement(prefix),
        )
        .await;
        let worker = proxy_pool(&settings, proxy.port).await;
        let delayed = Arc::new(ChatgptConnections::new(
            Some(worker.clone()),
            key,
            Ok(config),
        ));
        let owner = format!("deadline-complete-{}", uuid::Uuid::new_v4());
        let auth = start(&direct, &owner, None).await;
        let code = provider.issue_code(&auth, "deadline-subject", true);
        let stalled = tokio::spawn({
            let owner = owner.clone();
            let auth = auth.clone();
            let code = code.clone();
            async move {
                let mut request = callback(&auth, &code);
                if before_claim {
                    request.state = "invalid-state".into();
                }
                delayed
                    .operate(
                        &owner,
                        ConnectionOperation::Complete(
                            auth["authorization_id"].as_str().unwrap().into(),
                            request,
                        ),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
            .await
            .unwrap();
        // Give the competing owner a distinct deadline without sleeping in real time.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(85)).await;
        tokio::time::resume();
        let winner = if before_claim {
            provider
                .state
                .hold_code_exchange
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let task = tokio::spawn({
                let owner = owner.clone();
                let auth = auth.clone();
                let direct = direct.clone();
                async move {
                    direct
                        .operate(
                            &owner,
                            ConnectionOperation::Complete(
                                auth["authorization_id"].as_str().unwrap().into(),
                                callback(&auth, &code),
                            ),
                        )
                        .await
                }
            });
            tokio::time::timeout(
                Duration::from_secs(5),
                provider.state.code_exchange_started.notified(),
            )
            .await
            .unwrap();
            Some(task)
        } else {
            None
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), stalled)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(worker.get()),
        )
        .await
        .unwrap()
        .unwrap();
        let status: String = sqlx::query_scalar(
            "SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?",
        )
        .bind(&owner)
        .bind(auth["authorization_id"].as_str().unwrap())
        .fetch_one(shared.get())
        .await
        .unwrap();
        assert_eq!(status, if before_claim { "exchanging" } else { "failed" });
        proxy.gate.release.notify_one();
        if let Some(winner) = winner {
            provider.state.release_code_exchange.notify_one();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), winner)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_ok()
            );
        }
        worker.get().close().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn refresh_failure_cleanup_respects_its_budget() {
    use mysql_response_proxy::{Boundary, ResponseProxy};
    use std::time::Duration;
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    for (boundary, claim) in [
        (Boundary::Begin, true),
        (
            Boundary::Statement(b"UPDATE user_llm_connections SET refresh_owner=?"),
            true,
        ),
        (
            Boundary::Statement(b"UPDATE user_llm_connections SET status='reauth_required'"),
            false,
        ),
    ] {
        let provider = fixture::ProviderFixture::start().await;
        let key = Arc::new(FernetTokenEncryptor::new("review-refresh-cleanup").unwrap());
        let config = ChatgptConfig::for_test(&provider.base);
        let direct = Arc::new(ChatgptConnections::new(
            Some(shared.clone()),
            key.clone(),
            Ok(config.clone()),
        ));
        let owner = format!("review-refresh-cleanup-{}", uuid::Uuid::new_v4());
        let auth = start(&direct, &owner, None).await;
        let code = provider.issue_code(&auth, "cleanup-subject", true);
        let linked = operate_ok(
            &direct,
            &owner,
            ConnectionOperation::Complete(
                auth["authorization_id"].as_str().unwrap().into(),
                callback(&auth, &code),
            ),
        )
        .await;
        let id = linked["connection"]["connection_id"].as_str().unwrap();
        expire(shared.get(), &key, &owner, id).await;
        provider
            .state
            .refresh_error
            .store(if claim { 0 } else { 400 }, Ordering::SeqCst);
        let saved: String = sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?").bind(&owner).bind(id).fetch_one(shared.get()).await.unwrap();
        let proxy = ResponseProxy::start(settings.host.clone(), settings.port, boundary).await;
        let worker = proxy_pool(&settings, proxy.port).await;
        let delayed = Arc::new(ChatgptConnections::new(
            Some(worker.clone()),
            key,
            Ok(config),
        ));
        let binding = delayed.binding(&owner, id).await.unwrap();
        let waiting = tokio::spawn(async move { binding.acquire(None).await });
        tokio::time::timeout(Duration::from_secs(5), proxy.gate.received.notified())
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        let completed_before_release = waiting.is_finished();
        let healthy_before_release = tokio::time::timeout(
            Duration::from_millis(200),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(worker.get()),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        assert!(
            healthy_before_release,
            "pool must be healthy before releasing the failed settlement reply"
        );
        proxy.gate.release.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(worker.get()),
        )
        .await
        .unwrap()
        .unwrap();
        println!(
            "after 120s virtual time: refresh_complete={completed_before_release}, pool_healthy={healthy_before_release}; both recovered after releasing SQL"
        );
        if claim {
            assert_eq!(provider.state.refresh_calls.load(Ordering::SeqCst), 0);
            let retained: (String, Option<String>, String) = sqlx::query_as("SELECT credentials_encrypted,refresh_owner,status FROM user_llm_connections WHERE user_id=? AND connection_id=?").bind(&owner).bind(id).fetch_one(shared.get()).await.unwrap();
            assert_eq!(retained, (saved, None, "connected".into()));
        }
        worker.get().close().await;
        assert!(
            completed_before_release,
            "refresh failure cleanup exceeded the operation and lease budgets"
        );
    }
}

async fn proxy_pool(settings: &astra_core::MatrixOneSettings, port: u16) -> astra_core::SharedPool {
    let mut settings = settings.clone();
    settings.host = "127.0.0.1".into();
    settings.port = port;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 0;
    astra_core::SharedPool::new(&settings).await.unwrap()
}
