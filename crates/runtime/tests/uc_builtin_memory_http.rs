//! Native UC -> canonical Astra identity -> owner-scoped built-in memory.
//! Real MatrixOne persistence; deterministic UC/Memoria wire fixtures.
use astra_core::{
    JwtSettings, MatrixOneSettings, MemoriaSettings, SharedPool, config::UcNativeSettings,
};
use astra_memoria::MemoriaOperationError;
use astra_runtime::turn::memory_prefetch::{
    prefetch_memories_with_client, prefetch_session_start_memories_with_client,
};
use astra_runtime::{AppState, HealthChecker, MemoriaPort, ServiceInfo, build_app};
use astra_services::{
    AuthService, DatabaseAuthService, FernetTokenEncryptor, auth::uc::UcNativeProvider,
};
use astra_turn_types::MemoryRetrievalOutcome;
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

#[path = "../../services/tests/common/isolated_database.rs"]
mod isolated_database;

struct Healthy;
#[async_trait]
impl HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct Fixture {
    issuer: String,
    status: Arc<Mutex<String>>,
    writes: Arc<Mutex<Vec<String>>>,
    reads: Arc<Mutex<Vec<String>>>,
    authority_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[tokio::test]
#[ignore = "requires isolated ASTRA_TEST_DATABASE and ASTRA_TEST_DB_IT=1"]
async fn uc_builtin_memory_preserves_identity_lifecycle_and_transport() {
    let db = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&db.database);
    astra_services::storage::ensure_core_schema(&db, "mysql")
        .await
        .unwrap();
    let pool = SharedPool::new(&db).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fixture = Fixture {
        issuer: base.clone(),
        status: Arc::new(Mutex::new("active".into())),
        writes: Default::default(),
        reads: Default::default(),
        authority_calls: Default::default(),
    };
    let server = Router::new()
        .route("/protocol/openid-connect/token", post(|| async { Json(json!({"access_token":"service-only", "token_type":"Bearer", "expires_in":300})) }))
        .route("/api/v1/uc/internal/native-access-tokens/resolve", post(|State(f): State<Fixture>, headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer service-only");
            let token = body["access_token"].as_str().unwrap();
            let subject = token.rsplit('.').next().unwrap();
            Json(json!({"code":"OK", "data": {"issuer":f.issuer, "subject":subject,
                "session_id":"fixture-session", "client_id":"astra-cli", "audience":"astra-api",
                "expires_at":chrono::Utc::now().timestamp()+300, "email":"fixture@example.invalid", "display_name":"fixture"}}))
        }))
        .route("/api/v1/uc/internal/users/{subject}/status", get(|State(f): State<Fixture>, Path(subject): Path<String>, headers: HeaderMap| async move {
            f.authority_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(headers["authorization"], "Bearer service-only");
            let status = f.status.lock().unwrap().clone();
            if status == "unavailable" { return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({}))); }
            if status == "not-found" { return (StatusCode::NOT_FOUND, Json(json!({}))); }
            (StatusCode::OK, Json(json!({"code":"OK", "data":{"uc_sub": if status == "mismatch" { "wrong" } else { &subject }, "status":status}})))
        }))
        .route("/v1/memories", post(|State(f): State<Fixture>, headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Memoria-Owner test-deployment-key");
            let owner = headers["x-user-id"].to_str().unwrap();
            assert!(owner.starts_with("uc_") && owner.len() <= 64);
            assert!(body.get("user_id").is_none_or(|id| id == owner));
            f.writes.lock().unwrap().push(owner.into());
            Json(json!({"memory_id":"stored-memory", "user_id":owner}))
        }))
        .route("/v1/memories/retrieve", post(|State(f): State<Fixture>, headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Memoria-Owner test-deployment-key");
            let owner = headers["x-user-id"].to_str().unwrap();
            assert!(owner.starts_with("uc_") && owner.len() <= 64);
            f.reads.lock().unwrap().push(owner.into());
            Json(json!({"memories":[]}))
        })).with_state(fixture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    let settings = UcNativeSettings {
        issuer: base.clone(),
        adapter_url: base.clone(),
        client_secret: "test-service-secret".into(),
        moi_api_url: base.clone(),
        genesis_url: base.clone(),
        builtin_memory: true,
    };
    let memory = MemoriaSettings {
        base_url: base.clone(),
        master_key: Some("test-deployment-key".into()),
        self_hosted_master_access: false,
        issuer: None,
        web_url: None,
    };
    let auth = Arc::new(
        DatabaseAuthService::new(
            db.clone(),
            JwtSettings {
                secret_key: "test-jwt".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 15,
                refresh_token_expire_days: 7,
            },
        )
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new("test-uc-memory").unwrap())
        .with_uc_native(Some(UcNativeProvider::new(settings.clone()).unwrap()))
        .with_memoria_settings(&memory)
        .unwrap(),
    );
    let payload = URL_SAFE_NO_PAD.encode(json!({"iss":base}).to_string());
    let nonce = uuid::Uuid::new_v4();
    let alice_token = format!("header.{payload}.alice-{nonce}");
    let bob_token = format!("header.{payload}.bob-{nonce}");
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {alice_token}").parse().unwrap(),
    );
    let alice = auth.current_user(&headers).await.unwrap();
    headers.insert(
        "authorization",
        format!("Bearer {bob_token}").parse().unwrap(),
    );
    let bob = auth.current_user(&headers).await.unwrap();
    assert_eq!(alice.user_id.len(), 68);
    let make_port = |user: &str| {
        astra_runtime::turn::cloud::memoria_compact::UserScopedMemoriaPort::new(
            auth.memoria_credentials().unwrap(),
            user.into(),
        )
    };
    let alice_port = make_port(&alice.user_id);
    let bob_port = make_port(&bob.user_id);
    // Scoped-only resolution must not consult UC; an unavailable upstream
    // cannot change its absent-scoped-credential result.
    *fixture.status.lock().unwrap() = "unavailable".into();
    assert!(
        auth.memoria_credentials()
            .unwrap()
            .resolve(&alice.user_id)
            .await
            .unwrap()
            .is_none()
    );
    *fixture.status.lock().unwrap() = "active".into();
    let uc_provider_id = format!("uc:{}", settings.issuer);
    {
        let extra_subject = format!("ambiguous-{nonce}");
        sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?, ?, ?)")
            .bind(&uc_provider_id).bind(&extra_subject).bind(&alice.user_id)
            .execute(pool.get()).await.unwrap();
        assert!(alice_port.admits_operation(false).await.is_err());
        assert_eq!(
            prefetch_memories_with_client(&alice_port, "query", &alice.user_id, "session", 1)
                .await
                .outcome,
            MemoryRetrievalOutcome::Unavailable
        );
        assert!(fixture.reads.lock().unwrap().is_empty());
        sqlx::query("DELETE FROM auth_external_identities WHERE provider_id = ? AND external_subject = ? AND astra_user_id = ?")
            .bind(&uc_provider_id).bind(&extra_subject).bind(&alice.user_id)
            .execute(pool.get()).await.unwrap();
    }
    let subject: String = sqlx::query_scalar("SELECT external_subject FROM auth_external_identities WHERE provider_id = ? AND astra_user_id = ?")
        .bind(&uc_provider_id).bind(&alice.user_id).fetch_one(pool.get()).await.unwrap();
    sqlx::query("UPDATE auth_external_identities SET external_subject = '' WHERE provider_id = ? AND astra_user_id = ?")
        .bind(&uc_provider_id).bind(&alice.user_id).execute(pool.get()).await.unwrap();
    assert!(alice_port.admits_operation(false).await.is_err());
    assert!(matches!(
        alice_port.retrieve("query", None, 1).await,
        Err(MemoriaOperationError::AuthorityUnavailable(_))
    ));
    assert!(fixture.reads.lock().unwrap().is_empty());
    sqlx::query("UPDATE auth_external_identities SET external_subject = ? WHERE provider_id = ? AND astra_user_id = ?")
        .bind(subject).bind(&uc_provider_id).bind(&alice.user_id).execute(pool.get()).await.unwrap();
    let a = alice_port
        .resolve_tool_transport(true)
        .await
        .unwrap()
        .unwrap();
    let b = bob_port
        .resolve_tool_transport(true)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(a.owner_user_id, b.owner_user_id);
    assert!(a.owner_scoped_master && b.owner_scoped_master);
    let before = fixture
        .authority_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        prefetch_memories_with_client(&alice_port, "query", &alice.user_id, "session", 1)
            .await
            .outcome,
        MemoryRetrievalOutcome::Complete
    );
    assert_eq!(
        fixture
            .authority_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
    assert_eq!(
        *fixture.reads.lock().unwrap(),
        vec![a.owner_user_id.clone()]
    );
    let before = fixture
        .authority_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        prefetch_session_start_memories_with_client(&alice_port, &alice.user_id, "session")
            .await
            .outcome,
        MemoryRetrievalOutcome::Complete
    );
    assert_eq!(
        fixture
            .authority_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        before + 2
    );
    assert_eq!(fixture.reads.lock().unwrap().len(), 3);
    assert!(matches!(
        alice_port
            .retrieve_for_prompt("query", &bob.user_id, "session", 1)
            .await,
        Err(MemoriaOperationError::Failed(_))
    ));
    assert_eq!(fixture.reads.lock().unwrap().len(), 3);
    // Neither merely configuring a deployment key nor a different issuer
    // grants access to the previously authenticated UC account.
    for enabled in [false, true] {
        let mut isolated_settings = settings.clone();
        isolated_settings.builtin_memory = enabled;
        if enabled {
            isolated_settings.issuer.push_str("/another-issuer");
        }
        let isolated_auth = DatabaseAuthService::new(
            db.clone(),
            JwtSettings {
                secret_key: "test-jwt".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 15,
                refresh_token_expire_days: 7,
            },
        )
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new("test-uc-memory").unwrap())
        .with_uc_native(Some(UcNativeProvider::new(isolated_settings).unwrap()))
        .with_memoria_settings(&memory)
        .unwrap();
        let port = astra_runtime::turn::cloud::memoria_compact::UserScopedMemoriaPort::new(
            isolated_auth.memoria_credentials().unwrap(),
            alice.user_id.clone(),
        );
        assert!(!port.admits_operation(false).await.unwrap());
        assert!(matches!(
            port.retrieve("query", None, 1).await,
            Err(MemoriaOperationError::Disabled(_))
        ));
        assert_eq!(fixture.reads.lock().unwrap().len(), 3);
    }
    assert_eq!(
        alice_port
            .resolve_tool_transport(false)
            .await
            .unwrap()
            .unwrap()
            .owner_user_id,
        a.owner_user_id
    );
    assert!(alice_port.bind_owner(&bob.user_id).is_err());
    assert!(
        !make_port("unknown-user")
            .admits_operation(true)
            .await
            .unwrap()
    );
    assert_eq!(
        alice_port
            .store("test memory", "semantic", None, None)
            .await
            .unwrap(),
        "stored-memory"
    );

    // Public HTTP proxy must select the same owner and never return credentials.
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(pool.clone())
            .with_auth_service(auth.clone()),
    );
    let response = app.oneshot(Request::builder().method("POST").uri("/memory/store")
        .header("authorization", format!("Bearer {alice_token}")).header("content-type", "application/json")
        .header("x-user-id", &b.owner_user_id)
        .body(Body::from(json!({"content":"second memory", "memory_type":"semantic", "user_id":"attacker"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("test-deployment-key"));
    assert_eq!(
        *fixture.writes.lock().unwrap(),
        vec![a.owner_user_id.clone(), a.owner_user_id.clone()]
    );

    for status in ["disabled", "deleted", "pending_verification", "not-found"] {
        *fixture.status.lock().unwrap() = status.into();
        let before = fixture
            .authority_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            prefetch_session_start_memories_with_client(&alice_port, &alice.user_id, "session")
                .await
                .outcome,
            MemoryRetrievalOutcome::NotAttempted
        );
        assert_eq!(
            fixture
                .authority_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            before + 2,
            "disabled session-start lanes independently resolve current authority"
        );
        assert_eq!(fixture.reads.lock().unwrap().len(), 3);
        assert!(!alice_port.admits_operation(true).await.unwrap());
        assert!(
            alice_port
                .store("denied", "semantic", None, None)
                .await
                .is_err()
        );
    }
    for status in ["unavailable", "mismatch", "unknown-status"] {
        *fixture.status.lock().unwrap() = status.into();
        assert!(alice_port.admits_operation(false).await.is_err());
        assert_eq!(
            prefetch_memories_with_client(&alice_port, "query", &alice.user_id, "session", 1)
                .await
                .outcome,
            MemoryRetrievalOutcome::Unavailable
        );
        assert_eq!(fixture.reads.lock().unwrap().len(), 3);
    }
    *fixture.status.lock().unwrap() = "active".into();
    sqlx::query("UPDATE auth_users SET is_active = 0 WHERE user_id = ?")
        .bind(&alice.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(!alice_port.admits_operation(false).await.unwrap());
    assert!(matches!(
        alice_port.retrieve("query", None, 1).await,
        Err(MemoriaOperationError::Disabled(_))
    ));
    assert!(bob_port.admits_operation(false).await.unwrap());
    // A retained/disconnected Memoria identity cannot become a new built-in
    // grant, even if an operator has also mapped this account to UC.
    sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?, ?, ?)")
        .bind("memoria:retained-fixture").bind(format!("retained-{nonce}")).bind(&bob.user_id)
        .execute(pool.get()).await.unwrap();
    assert!(!bob_port.admits_operation(true).await.unwrap());
    assert!(matches!(
        bob_port.retrieve("query", None, 1).await,
        Err(MemoriaOperationError::Disabled(_))
    ));
    sqlx::query("DELETE FROM auth_external_identities WHERE astra_user_id = ?")
        .bind(&bob.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(!bob_port.admits_operation(false).await.unwrap());
    assert!(matches!(
        bob_port.retrieve("query", None, 1).await,
        Err(MemoriaOperationError::Disabled(_))
    ));
    assert_eq!(fixture.reads.lock().unwrap().len(), 3);
    assert_eq!(fixture.writes.lock().unwrap().len(), 2);
    server.abort();
}
