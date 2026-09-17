mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;
use astra_core::JwtSettings;
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService,
    auth::{AuthRefreshRequestData, AuthService, DatabaseAuthService},
};
use axum::{
    Json, Router,
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::get,
};
use serde_json::json;
use sha2::Digest;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn memoria_refresh_revocation_and_deployment_model_isolation() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get();
    let owner = Uuid::new_v4().to_string();
    let key_id = Uuid::new_v4().to_string();
    let revoked = Arc::new(AtomicBool::new(false));
    let flag = revoked.clone();
    let whoami = json!({"user_id":owner, "key_id":key_id, "is_active":true, "is_master":false,
        "scope":{"type":"personal","id":owner},"api_version":"1",
        "capabilities":["api_key_scopes","memory_filters_v1"],"granted_scopes":["identity:read"]});
    let app = Router::new().route(
        "/auth/whoami",
        get(move || {
            let flag = flag.clone();
            let body = whoami.clone();
            async move {
                (
                    if flag.load(Ordering::SeqCst) {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::OK
                    },
                    Json(body),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let encryptor = FernetTokenEncryptor::new("test-only-key").unwrap();
    let jwt = JwtSettings {
        secret_key: "test-only-jwt-secret".into(),
        algorithm: "HS256".into(),
        access_token_expire_minutes: 90,
        refresh_token_expire_days: 7,
    };
    let auth = DatabaseAuthService::new(settings.clone(), jwt.clone())
        .with_pool(shared.clone())
        .with_encryptor(encryptor.clone())
        .with_memoria_base_url(url);
    let tokens = auth.login_memoria("test-key").await.unwrap().tokens;
    assert_eq!(tokens.expires_in, 900);

    // Actual JWT -> stored refresh session -> mapping -> stored credential -> HTTP.
    let refreshed = auth
        .refresh(AuthRefreshRequestData {
            refresh_token: tokens.refresh_token,
        })
        .await
        .unwrap();
    assert_eq!(refreshed.expires_in, 900);
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", refreshed.access_token)).unwrap(),
    );
    auth.current_user(&headers).await.unwrap();
    revoked.store(true, Ordering::SeqCst);
    assert_eq!(
        auth.refresh(AuthRefreshRequestData {
            refresh_token: refreshed.refresh_token.clone()
        })
        .await
        .unwrap_err()
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Legacy `internal` tokens cannot evade refresh validation or the 15-min
    // access window when the self-hosted JWT configuration used a longer TTL.
    let decoded: jsonwebtoken::TokenData<serde_json::Value> = jsonwebtoken::decode(
        &refreshed.access_token,
        &jsonwebtoken::DecodingKey::from_secret(jwt.secret_key.as_bytes()),
        &jsonwebtoken::Validation::default(),
    )
    .unwrap();
    for kind in ["refresh", "access"] {
        let mut claims = decoded.claims.clone();
        claims["origin"] = json!("internal");
        claims["type"] = json!(kind);
        claims["iat"] = json!(chrono::Utc::now().timestamp() - 901);
        claims["exp"] = json!(chrono::Utc::now().timestamp() + 3600);
        let legacy = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(jwt.secret_key.as_bytes()),
        )
        .unwrap();
        if kind == "refresh" {
            use sha2::{Digest, Sha256};
            sqlx::query("INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,is_revoked) VALUES (?, ?, ?, ?, DATE_ADD(NOW(), INTERVAL 1 DAY), 0)")
                .bind(Uuid::new_v4().to_string()).bind(&tokens.user_id).bind(claims["sid"].as_str().unwrap())
                .bind(format!("{:x}", Sha256::digest(legacy.as_bytes()))).execute(pool).await.unwrap();
            assert_eq!(
                auth.refresh(AuthRefreshRequestData {
                    refresh_token: legacy
                })
                .await
                .unwrap_err()
                .0,
                StatusCode::UNAUTHORIZED
            );
        } else {
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {legacy}")).unwrap(),
            );
            assert_eq!(
                auth.current_user(&headers).await.unwrap_err().0,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    let deployment = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO infra_llm_models (model_id,model_name,provider,base_url,is_active,context_window,api_key_encrypted,input_modalities,output_modalities,supported_parameters,pricing,tags,quirks) VALUES (?, ?, 'mock', 'http://127.0.0.1:1', 1, 128000, ?, ?, ?, '[]', '{}', '[]', '{}')")
        .bind(&deployment).bind(format!("deployment-{deployment}"))
        .bind(encryptor.encrypt("deployment-test-secret").unwrap()).bind(r#"["text"]"#).bind(r#"["text"]"#)
        .execute(pool).await.unwrap();
    let models = DatabaseModelService::new(settings, Arc::new(encryptor)).with_pool(shared.clone());
    assert!(
        !models
            .allows_deployment_models(tokens.user_id.clone())
            .await
            .unwrap()
    );
    assert!(
        models
            .list_models(tokens.user_id.clone(), false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        models
            .admit_model_offering(tokens.user_id.clone(), deployment.clone())
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    // The test is run in both deployment modes: non-Memoria accounts keep the
    // original self-hosted behavior, but cannot bypass explicit cloud-byok.
    let self_hosted = std::env::var("ASTRA_DEPLOYMENT_MODE").as_deref() != Ok("cloud-byok");
    assert_eq!(
        models
            .allows_deployment_models("local-test-user".into())
            .await
            .unwrap(),
        self_hosted
    );
    assert_eq!(
        models
            .admit_model_offering("local-test-user".into(), deployment.clone())
            .await
            .is_ok(),
        self_hosted
    );
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(deployment)
        .execute(pool)
        .await
        .unwrap();
    for table in [
        "auth_tokens",
        "auth_refresh_tokens",
        "auth_user_roles",
        "auth_memoria_identities",
        "auth_external_identities",
        "auth_users",
    ] {
        let column = match table {
            "auth_tokens" => "scope_user_id",
            "auth_memoria_identities" | "auth_external_identities" => "astra_user_id",
            _ => "user_id",
        };
        sqlx::query(&format!("DELETE FROM {table} WHERE {column} = ?"))
            .bind(&tokens.user_id)
            .execute(pool)
            .await
            .unwrap();
    }
    server.abort();
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn memoria_issuer_atomicity_concurrent_binding_and_disconnect() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (shared, db) = common::setup_pool_and_settings().await;
    let subject = format!("review-{}", Uuid::new_v4());
    let subject_for_http = subject.clone();
    let revoked = Arc::new(AtomicBool::new(false));
    let flag = revoked.clone();
    let app = Router::new().route("/auth/whoami", get(move |headers: HeaderMap| {
        let subject = subject_for_http.clone();
        let revoked = flag.load(Ordering::SeqCst);
        async move {
            let key = headers.get("authorization").and_then(|h| h.to_str().ok())
                .unwrap_or("").trim_start_matches("Bearer ");
            let owner = match key {
                "atomic-failure" => format!("{subject}-failure"),
                "legacy-key" => format!("{subject}-legacy"),
                _ => subject,
            };
            (if revoked { StatusCode::UNAUTHORIZED } else { StatusCode::OK },
                Json(json!({"user_id": owner, "key_id": key, "is_active": true, "is_master": false,
                    "scope": {"type": "personal", "id": owner}, "api_version": "1",
                    "capabilities": ["api_key_scopes", "memory_filters_v1"],
                    "granted_scopes": ["identity:read", "memory:read"]})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let settings = astra_core::MemoriaSettings {
        base_url: base.clone(),
        master_key: None,
        self_hosted_master_access: false,
        issuer: None,
        web_url: Some("http://localhost".into()),
        legacy_issuer: None,
    };
    let jwt = JwtSettings {
        secret_key: "review-704-jwt".into(),
        algorithm: "HS256".into(),
        access_token_expire_minutes: 90,
        refresh_token_expire_days: 7,
    };
    let encryptor = FernetTokenEncryptor::new("review-704-encryption").unwrap();
    let auth = DatabaseAuthService::new(db.clone(), jwt.clone())
        .with_pool(shared.clone())
        .with_encryptor(encryptor.clone())
        .with_memoria_settings(&settings)
        .unwrap();
    let mut logins = Vec::new();
    for i in 0..6 {
        let auth = auth.clone();
        logins.push(tokio::spawn(async move {
            auth.login_memoria(&format!("key-{i}")).await
        }));
    }
    let mut tokens = Vec::new();
    for login in logins {
        tokens.push(login.await.unwrap().unwrap().tokens);
    }
    let user = tokens[0].user_id.clone();
    assert!(tokens.iter().all(|t| t.user_id == user));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_tokens WHERE type = 'memoria_connection' AND scope_user_id = ? AND is_active = 1")
        .bind(&user).fetch_one(shared.get()).await.unwrap();
    assert_eq!(
        count, 1,
        "one durable binding, even for concurrent first login/relink"
    );
    let identities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_external_identities WHERE external_subject = ?",
    )
    .bind(&subject)
    .fetch_one(shared.get())
    .await
    .unwrap();
    assert_eq!(identities, 1);
    let resolver = auth.memoria_credentials().unwrap();
    let grant = resolver.resolve(&user).await.unwrap().unwrap();
    assert_eq!(
        grant.access,
        astra_services::auth::memoria::MemoryAccess::ReadOnly
    );
    assert!(grant.generation.starts_with("key-"));
    assert_eq!(grant.owner, subject);
    let encrypted: String = sqlx::query_scalar("SELECT encrypted_value FROM auth_tokens WHERE type = 'memoria_connection' AND scope_user_id = ?")
        .bind(&user).fetch_one(shared.get()).await.unwrap();
    assert_ne!(encrypted, grant.key);
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", tokens[0].access_token)).unwrap(),
    );
    assert!(matches!(
        auth.current_principal(&headers).await.unwrap().origin,
        astra_services::AuthPrincipalOrigin::VerifiedProvider { .. }
    ));

    // Losing a provider mapping must not downgrade a verified JWT to an
    // internal principal while its session row is still active.
    sqlx::query(
        "DELETE FROM auth_external_identities WHERE provider_id = ? AND external_subject = ?",
    )
    .bind(&resolver.provider.provider_id)
    .bind(&subject)
    .execute(shared.get())
    .await
    .unwrap();
    assert_eq!(
        auth.current_user(&headers).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("INSERT INTO auth_external_identities (provider_id, external_subject, astra_user_id) VALUES (?, ?, ?)")
        .bind(&resolver.provider.provider_id).bind(&subject).bind(&user)
        .execute(shared.get()).await.unwrap();

    // Same subject at a different issuer is a different account, and cannot
    // refresh / access a session from the old issuer after configuration changes.
    let other = DatabaseAuthService::new(db.clone(), jwt.clone())
        .with_pool(shared.clone())
        .with_encryptor(encryptor.clone())
        .with_memoria_settings(&astra_core::MemoriaSettings {
            issuer: Some("https://another-issuer.example".into()),
            ..settings.clone()
        })
        .unwrap();
    let other_login = other.login_memoria("key-other").await.unwrap();
    assert_ne!(other_login.tokens.user_id, user);
    assert_eq!(
        other.current_user(&headers).await.err().unwrap().0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        other
            .refresh(AuthRefreshRequestData {
                refresh_token: tokens[0].refresh_token.clone()
            })
            .await
            .err()
            .unwrap()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        other
            .memoria_credentials()
            .unwrap()
            .resolve(&user)
            .await
            .unwrap()
            .is_none()
    );

    // Token creation fails AFTER identity and credential SQL, but rolls them
    // both back. This is not a fixture that manually seeds an already valid grant.
    let bad = DatabaseAuthService::new(
        db.clone(),
        JwtSettings {
            algorithm: "unsupported".into(),
            ..jwt
        },
    )
    .with_pool(shared.clone())
    .with_encryptor(encryptor)
    .with_memoria_settings(&settings)
    .unwrap();
    assert!(bad.login_memoria("atomic-failure").await.is_err());
    let orphan_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_external_identities WHERE external_subject = ?",
    )
    .bind(format!("{subject}-failure"))
    .fetch_one(shared.get())
    .await
    .unwrap();
    assert_eq!(
        orphan_count, 0,
        "failed login cannot leave an identity or its account"
    );
    let failed_user = format!(
        "ext_{:x}",
        sha2::Sha256::digest(
            format!("{}\0{subject}-failure", resolver.provider.provider_id).as_bytes()
        )
    );
    for (table, column) in [
        ("auth_users", "user_id"),
        ("auth_tokens", "scope_user_id"),
        ("auth_refresh_tokens", "user_id"),
    ] {
        let rows: i64 =
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?"))
                .bind(&failed_user)
                .fetch_one(shared.get())
                .await
                .unwrap();
        assert_eq!(rows, 0, "failed login left rows in {table}");
    }

    // Legacy subjects have no issuer. They are never silently assigned to
    // whichever instance happens to answer the next login.
    let legacy_user = format!("legacy-{}", Uuid::new_v4());
    let legacy_subject = format!("{subject}-legacy");
    sqlx::query("INSERT INTO auth_users (user_id,username,email,password_hash,is_active) VALUES (?, ?, ?, '', 1)")
        .bind(&legacy_user).bind(&legacy_user).bind(format!("{legacy_user}@test.invalid"))
        .execute(shared.get()).await.unwrap();
    sqlx::query(
        "INSERT INTO auth_memoria_identities (memoria_user_id,astra_user_id) VALUES (?, ?)",
    )
    .bind(&legacy_subject)
    .bind(&legacy_user)
    .execute(shared.get())
    .await
    .unwrap();
    assert_eq!(
        auth.login_memoria("legacy-key").await.err().unwrap().0,
        StatusCode::CONFLICT
    );
    let migrator = auth
        .clone()
        .with_memoria_settings(&astra_core::MemoriaSettings {
            legacy_issuer: Some(base),
            ..settings.clone()
        })
        .unwrap();
    assert_eq!(
        migrator
            .login_memoria("legacy-key")
            .await
            .unwrap()
            .tokens
            .user_id,
        legacy_user
    );
    assert_eq!(
        migrator
            .login_memoria("legacy-key")
            .await
            .unwrap()
            .tokens
            .user_id,
        legacy_user
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_memoria_identities WHERE astra_user_id = ?")
            .bind(&legacy_user)
            .fetch_one(shared.get())
            .await
            .unwrap();
    assert_eq!(remaining, 0);
    migrator.disconnect_memoria(&legacy_user).await.unwrap();

    auth.disconnect_memoria(&user).await.unwrap();
    auth.disconnect_memoria(&user).await.unwrap(); // idempotent service operation
    assert!(resolver.resolve(&user).await.unwrap().is_none());
    assert!(matches!(
        resolver.resolve_runtime(&user).await.unwrap(),
        astra_services::auth::memoria::MemoriaCredentialResolution::Denied
    ));
    assert_eq!(
        auth.current_user(&headers).await.err().unwrap().0,
        StatusCode::UNAUTHORIZED
    );
    for token in &tokens {
        assert_eq!(
            auth.refresh(AuthRefreshRequestData {
                refresh_token: token.refresh_token.clone()
            })
            .await
            .err()
            .unwrap()
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let relink = auth.login_memoria("new-key").await.unwrap();
    assert_eq!(
        relink.tokens.user_id, user,
        "unlink must not destroy account continuity"
    );
    revoked.store(true, Ordering::SeqCst);
    assert_eq!(
        auth.refresh(AuthRefreshRequestData {
            refresh_token: relink.tokens.refresh_token
        })
        .await
        .err()
        .unwrap()
        .0,
        StatusCode::UNAUTHORIZED
    );
    auth.disconnect_memoria(&user).await.unwrap();
    other
        .disconnect_memoria(&other_login.tokens.user_id)
        .await
        .unwrap();
    // Account deactivation blocks existing runtime bindings immediately, and
    // maintenance removes the encrypted secret without deleting Work/history.
    revoked.store(false, Ordering::SeqCst);
    auth.login_memoria("retention-key").await.unwrap();
    sqlx::query("UPDATE auth_users SET is_active = 0 WHERE user_id = ?")
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
    assert!(resolver.resolve(&user).await.unwrap().is_none());
    assert!(matches!(
        resolver.resolve_runtime(&user).await.unwrap(),
        astra_services::auth::memoria::MemoriaCredentialResolution::Denied
    ));
    assert_eq!(
        auth.login_memoria("retention-key").await.err().unwrap().0,
        StatusCode::FORBIDDEN
    );
    astra_services::storage::cleanup_inactive_memoria_credentials(shared.get())
        .await
        .unwrap();
    let secrets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_tokens WHERE scope_user_id = ? AND type = 'memoria_connection'",
    )
    .bind(&user)
    .fetch_one(shared.get())
    .await
    .unwrap();
    assert_eq!(secrets, 0);
    server.abort();
}
