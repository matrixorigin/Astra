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
    let catalog = models.list_models(owner.clone(), false).await.unwrap();
    let own: Vec<_> = catalog
        .iter()
        .filter(|item| model_ids.contains(&item.offering_id))
        .collect();
    assert_eq!(own.len(), 2);
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
    let (shared, settings) = common::setup_pool_and_settings().await;
    isolated_database::require_isolated_database(&settings.database);
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
    let (shared, mut settings) = common::setup_pool_and_settings().await;
    isolated_database::require_isolated_database(&settings.database);
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
