use astra_core::{JwtSettings, MatrixOneSettings, MemoriaSettings, SharedPool};
use astra_runtime::{AppState, HealthChecker, MemoriaPort, ServiceInfo, build_app};
use astra_services::{
    AuthService, DatabaseAuthService, FernetTokenEncryptor, auth::AuthRegisterRequestData,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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

async fn request(
    app: Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[tokio::test]
#[ignore = "requires isolated ASTRA_TEST_DATABASE and ASTRA_TEST_DB_IT=1"]
async fn public_memoria_auth_uses_one_provider_and_enforces_disconnect() {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let db = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&db.database);
    astra_services::storage::ensure_core_schema(&db, "mysql")
        .await
        .unwrap();
    let pool = SharedPool::new(&db).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let owner = format!("http-{}", uuid::Uuid::new_v4());
    let read_calls = calls.clone();
    let store_calls = calls.clone();
    let app = Router::new()
        .route("/auth/whoami", get(move |headers: axum::http::HeaderMap| {
            let owner = owner.clone();
            async move {
                let key = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                let scopes = if key == "Bearer readonly-key" {
                    vec!["identity:read", "memory:read"]
                } else { vec!["identity:read"] };
                (if key == "Bearer invalid-key" { StatusCode::UNAUTHORIZED } else { StatusCode::OK },
                Json(json!({"user_id":owner, "key_id":key, "is_active":true, "is_master":false,
                    "scope":{"type":"personal","id":owner}, "api_version":"1",
                    "capabilities":["api_key_scopes","memory_filters_v1"], "granted_scopes":scopes})))
            }
        }))
        .route(
            "/v1/profiles/me",
            get(move |headers: axum::http::HeaderMap| {
                read_calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(headers["authorization"], "Bearer readonly-key");
                async { Json(json!({"profile":"from-provider-a"})) }
            }),
        )
        .route(
            "/v1/memories",
            post(move |headers: axum::http::HeaderMap| {
                store_calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    headers["authorization"],
                    "Memoria-Owner self-hosted-fallback-key"
                );
                async { Json(json!({"memory_id":"local-memory"})) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = MemoriaSettings {
        base_url: base.clone(),
        master_key: None,
        self_hosted_master_access: false,
        issuer: None,
        web_url: Some("http://localhost".into()),
        legacy_issuer: None,
    };
    let auth = Arc::new(
        DatabaseAuthService::new(
            db,
            JwtSettings {
                secret_key: "review-http-auth".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 30,
                refresh_token_expire_days: 7,
            },
        )
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new("review-http-encryption").unwrap())
        .with_memoria_settings(&provider)
        .unwrap(),
    );
    let local = auth
        .register(AuthRegisterRequestData {
            username: format!("local-{}", uuid::Uuid::new_v4()),
            email: format!("local-{}@example.invalid", uuid::Uuid::new_v4()),
            password: "Local-password-1".into(),
            display_name: None,
        })
        .await
        .unwrap();
    let local_port = astra_runtime::turn::cloud::memoria_compact::UserScopedMemoriaPort::new(
        auth.memoria_credentials().unwrap(),
        local.user_id.clone(),
    )
    .with_self_hosted_fallback(base.clone(), "self-hosted-fallback-key".into());
    assert!(local_port.admits_operation(true).await.unwrap());
    let local_transport = local_port
        .resolve_tool_transport(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(local_transport.owner_user_id, local.user_id);
    assert!(local_transport.owner_scoped_master);
    assert_eq!(
        local_port
            .store("local", "semantic", None, None)
            .await
            .unwrap(),
        "local-memory"
    );
    // Even with self-hosted fallback enabled, the persisted scoped binding
    // remains authoritative for its owner and consent mode.
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(pool.clone())
            .with_auth_service(auth.clone())
            .with_memoria_config(
                "http://127.0.0.1:1",
                Some("self-hosted-fallback-key".into()),
            )
            .with_self_hosted_memoria_fallback(true),
    );

    let (status, methods) = request(app.clone(), "GET", "/auth/methods", None, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(methods["memoria"]["authorization_url"], "http://localhost");
    assert_eq!(methods["memoria"]["issuer"], provider.base_url);
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/memoria",
            None,
            json!({"connection_key":""})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/memoria",
            None,
            json!({"connection_key":"invalid-key"})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, login) = request(
        app.clone(),
        "POST",
        "/auth/memoria",
        None,
        json!({"connection_key":"identity-key"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    assert_eq!(login["memory_access"], "none");
    assert!(!login.to_string().contains("identity-key"));
    let token = login["access_token"].as_str().unwrap();
    let (status, denied) = request(
        app.clone(),
        "GET",
        "/memory/profile",
        Some(token),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["error_code"], "memory_consent_denied");
    assert!(
        denied["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (status, relink) = request(
        app.clone(),
        "POST",
        "/auth/memoria",
        None,
        json!({"connection_key":"readonly-key"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(relink["user_id"], login["user_id"]);
    let retained_scoped_port =
        astra_runtime::turn::cloud::memoria_compact::UserScopedMemoriaPort::new(
            auth.memoria_credentials().unwrap(),
            login["user_id"].as_str().unwrap().to_string(),
        )
        .with_self_hosted_fallback(base, "self-hosted-fallback-key".into());
    assert!(retained_scoped_port.admits_operation(false).await.unwrap());
    assert!(!retained_scoped_port.admits_operation(true).await.unwrap());
    let scoped_transport = retained_scoped_port
        .resolve_tool_transport(false)
        .await
        .unwrap()
        .unwrap();
    assert!(!scoped_transport.owner_scoped_master);
    assert_ne!(scoped_transport.owner_user_id, login["user_id"]);
    let (status, profile) = request(
        app.clone(),
        "GET",
        "/memory/profile",
        Some(token),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{profile}");
    assert_eq!(profile["profile"], "from-provider-a");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let (status, denied) = request(
        app.clone(),
        "POST",
        "/memory/store",
        Some(token),
        json!({"content":"blocked","memory_type":"semantic"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["error_code"], "memory_consent_denied");
    assert_eq!(
        request(app.clone(), "DELETE", "/auth/memoria", None, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            "/auth/memoria",
            Some(token),
            json!({})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/refresh",
            None,
            json!({"refresh_token":login["refresh_token"]})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/memory/profile", Some(token), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        auth.memoria_credentials()
            .unwrap()
            .resolve(login["user_id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    let calls_after_disconnect = calls.load(Ordering::SeqCst);
    assert!(!retained_scoped_port.admits_operation(false).await.unwrap());
    assert!(
        retained_scoped_port
            .resolve_tool_transport(false)
            .await
            .is_err()
    );
    assert!(
        retained_scoped_port
            .store("revoked", "semantic", None, None)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), calls_after_disconnect);

    sqlx::query("UPDATE auth_users SET is_active = 0 WHERE user_id = ?")
        .bind(&local.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(!local_port.admits_operation(true).await.unwrap());
    assert!(local_port.resolve_tool_transport(true).await.is_err());
    assert!(
        local_port
            .store("inactive", "semantic", None, None)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), calls_after_disconnect);
    sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
        .bind(&local.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(!local_port.admits_operation(false).await.unwrap());
    assert!(local_port.resolve_tool_transport(false).await.is_err());
    assert!(
        local_port
            .store("deleted", "semantic", None, None)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), calls_after_disconnect);
    server.abort();
}
