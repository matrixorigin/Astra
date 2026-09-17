//! Background extraction must obey the same owner/model boundary as chat.
use astra_core::{JwtSettings, MatrixOneSettings, SharedPool};
use astra_runtime::{
    matrix_cloud_runtime::PoolMemoryInferenceResolver,
    memory_hooks::MemoryInferenceRequest,
    session_memory::{
        BackgroundActivityBroker, ExtractionRequest, MemoryExtractionService,
        MemoryInferenceResolver, SpawnDecision,
    },
    turn::cloud::memoria_compact::{MemoriaPort, UserScopedMemoriaPort},
};
use astra_services::{
    AuthService, DatabaseAuthService, FernetTokenEncryptor, event_ingestion::IngestionSender,
};
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[path = "../../services/tests/common/isolated_database.rs"]
mod isolated_database;

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn readwrite_memoria_extraction_cannot_spend_deployment_credentials() {
    let settings = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&settings.database);
    astra_services::storage::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = SharedPool::new(&settings).await.unwrap();
    let encryptor = Arc::new(FernetTokenEncryptor::new("memory-owner-test-encryption").unwrap());
    let subject = uuid::Uuid::new_v4().to_string();
    let whoami = json!({"user_id":subject, "key_id":"fresh-rw-key", "is_active":true, "is_master":false,
        "scope":{"type":"personal","id":subject}, "api_version":"1",
        "capabilities":["api_key_scopes","memory_filters_v1"], "granted_scopes":["identity:read","memory:read","memory:write"]});
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = hits.clone();
    let router = Router::new()
        .route("/auth/whoami", get(move || { let whoami = whoami.clone(); async { Json(whoami) } }))
        .route("/v1/chat/completions", post(move || {
            requests.fetch_add(1, Ordering::SeqCst);
            async { Json(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"NO_CHANGE"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})) }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let auth = DatabaseAuthService::new(
        settings.clone(),
        JwtSettings {
            secret_key: "memory-owner-test-jwt".into(),
            algorithm: "HS256".into(),
            access_token_expire_minutes: 15,
            refresh_token_expire_days: 1,
        },
    )
    .with_pool(pool.clone())
    .with_encryptor((*encryptor).clone())
    .with_memoria_base_url(base.clone());
    let login = auth.login_memoria("rw-test-key").await.unwrap();
    let user = login.tokens.user_id;
    let memoria = Arc::new(UserScopedMemoriaPort::new(
        auth.memoria_credentials().unwrap(),
        user.clone(),
    ));
    assert!(
        memoria.admits_operation(true).await.unwrap(),
        "test must exercise read-write consent"
    );
    let model_id = uuid::Uuid::new_v4().to_string();
    let model_name = format!("selector-{model_id}");
    sqlx::query("INSERT INTO infra_llm_models (model_id,model_name,provider,base_url,is_active,context_window,api_key_encrypted,input_modalities,output_modalities,supported_parameters,pricing,tags,quirks) VALUES (?, ?, 'openai', ?, 1, 128000, ?, '[\"text\"]', '[\"text\"]', '[]', '{}', '[\"selector\"]', '{}')")
        .bind(&model_id).bind(&model_name).bind(format!("{base}/v1")).bind(encryptor.encrypt("deployment-only-key").unwrap())
        .execute(pool.get()).await.unwrap();
    let resolver = Arc::new(PoolMemoryInferenceResolver::new(pool.clone(), encryptor));
    assert!(resolver.resolve_candidates(&user).await.is_empty());
    let scope = |owner: &str| InferenceInvocationScope::Session {
        session_id: owner.into(),
        turn: 1,
        round: 0,
        operation_id: uuid::Uuid::new_v4().to_string(),
        logical_attempt: 0,
    };
    let messages = vec![
        json!({"role":"user","content":"Remember that this project requires regression tests before release."}),
        json!({"role":"assistant","content":"I will add tests and verify the implementation."}),
    ];
    let (ingestion, mut events) = IngestionSender::for_tests(64);
    let service = Arc::new(MemoryExtractionService::new(
        resolver.clone(),
        memoria,
        ingestion,
        user.clone(),
        Arc::new(BackgroundActivityBroker::new()),
    ));
    let memory_session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (session_id,user_id) VALUES (?, ?)")
        .bind(&memory_session)
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
    assert_eq!(
        service.maybe_spawn(ExtractionRequest {
            inference_scope: scope(&memory_session),
            messages: messages.clone(),
            session_facts: Default::default(),
            had_error: false,
            reanchors_current_objective: false,
        }),
        SpawnDecision::Spawned
    );
    assert_eq!(service.wait_for_pending(Duration::from_secs(10)).await, 0);
    assert!(
        events.try_recv().is_ok(),
        "extraction must emit its deterministic/degraded outcome"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "memory consent must not authorize deployment inference"
    );

    let control = format!("local-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO auth_users (user_id, username, email, password_hash, is_active) VALUES (?, ?, ?, '', 1)")
        .bind(&control).bind(&control).bind(format!("{control}@test.invalid")).execute(pool.get()).await.unwrap();
    let candidates = resolver.resolve_candidates(&control).await;
    if std::env::var("ASTRA_DEPLOYMENT_MODE").as_deref() == Ok("cloud-byok") {
        assert!(candidates.is_empty());
    } else {
        let client = candidates
            .iter()
            .find(|c| c.model_name() == model_name)
            .expect("permitted self-hosted selector");
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id) VALUES (?, ?)")
            .bind(&control)
            .bind(&control)
            .execute(pool.get())
            .await
            .unwrap();
        let control_scope = scope(&control);
        let request = MemoryInferenceRequest {
            purpose: InferencePurpose::MemoryExtraction,
            invocation_scope: &control_scope,
            messages: &messages,
            max_output_tokens: 128,
            temperature: 0.0,
            deadline: Duration::from_secs(5),
        };
        assert_eq!(client.complete(request).await.unwrap(), "NO_CHANGE");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // An already constructed background client must revalidate before I/O.
        sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES ('memoria:test-background-owner', ?, ?)")
            .bind(&control).bind(&control).execute(pool.get()).await.unwrap();
        assert!(client.complete(request).await.is_err());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "owner eligibility changed but cached client sent another request"
        );
        sqlx::query("DELETE FROM auth_external_identities WHERE astra_user_id=?")
            .bind(&control)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("UPDATE infra_llm_models SET is_active=0 WHERE model_id=?")
            .bind(&model_id)
            .execute(pool.get())
            .await
            .unwrap();
        assert!(client.complete(request).await.is_err());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "revoked model received another request"
        );
    }
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id=?")
        .bind(&model_id)
        .execute(pool.get())
        .await
        .unwrap();
    auth.disconnect_memoria(&user).await.unwrap();
    server.abort();
}
