mod common;

use astra_core::{JwtSettings, SharedPool};
use astra_services::{
    AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    BrowserSessionRequest, DatabaseAuthService, FernetTokenEncryptor,
};
use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::Arc;

#[path = "common/browser_session.rs"]
mod fixture;
use fixture::{ENCRYPTION_KEY, JWT_KEY, digest, expire_access};
fn handle(tokens: &astra_services::AuthTokenRecord) -> String {
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let payload = tokens.refresh_token.split('.').nth(1).unwrap();
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    let sid = claims["sid"].as_str().unwrap();
    format!(
        "bs3.{}.{}.{}.{}",
        URL_SAFE_NO_PAD.encode("http://localhost:17001/"),
        URL_SAFE_NO_PAD.encode(&tokens.user_id),
        URL_SAFE_NO_PAD.encode(sid),
        URL_SAFE_NO_PAD.encode(&secret.as_bytes()[..32])
    )
}
fn refresh(value: &str) -> AuthRefreshRequestData {
    AuthRefreshRequestData {
        refresh_token: value.into(),
    }
}
fn adoption(value: &str, tokens: &astra_services::AuthTokenRecord) -> BrowserSessionRequest {
    BrowserSessionRequest {
        handle: value.into(),
        refresh_token: tokens.refresh_token.clone(),
        access_token: tokens.access_token.clone(),
    }
}

async fn login(service: &DatabaseAuthService, name: &str) -> astra_services::AuthTokenRecord {
    service
        .login(AuthLoginRequestData {
            username: name.into(),
            password: "browser-test-password".into(),
        })
        .await
        .unwrap()
}

async fn register(service: &DatabaseAuthService) -> String {
    let name = format!("browser_{}", uuid::Uuid::new_v4().simple());
    service
        .register(AuthRegisterRequestData {
            username: name.clone(),
            email: format!("{name}@example.test"),
            password: "browser-test-password".into(),
            display_name: None,
        })
        .await
        .unwrap();
    name
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn browser_sessions_converge_across_pools_and_preserve_revocation_and_replay_boundaries() {
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let pool = isolated.pool.clone();
    let settings = isolated.settings.clone();
    isolated.run(browser_sessions_converge_across_pools_and_preserve_revocation_and_replay_boundaries_body(pool, settings)).await;
}

async fn browser_sessions_converge_across_pools_and_preserve_revocation_and_replay_boundaries_body(
    pool: astra_core::SharedPool,
    settings: astra_core::MatrixOneSettings,
) {
    let jwt = JwtSettings {
        secret_key: JWT_KEY.into(),
        algorithm: "HS256".into(),
        access_token_expire_minutes: 5,
        refresh_token_expire_days: 1,
    };
    let make_service = |p: SharedPool| {
        DatabaseAuthService::new(settings.clone(), jwt.clone())
            .with_pool(p)
            .with_encryptor(FernetTokenEncryptor::new(ENCRYPTION_KEY).unwrap())
    };
    let first = Arc::new(make_service(pool.clone()));
    let second = Arc::new(make_service(SharedPool::new(&settings).await.unwrap()));
    let name = register(&first).await;
    // A drifted canonical primary key must not consume a usable login token.
    let drifted_name = register(&first).await;
    let drifted = login(&first, &drifted_name).await;
    let drifted_handle = handle(&drifted);
    let signed_sid = String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(drifted_handle.split('.').nth(3).unwrap())
            .unwrap(),
    )
    .unwrap();
    let moved_sid = uuid::Uuid::new_v4().to_string();
    let moved = sqlx::query("UPDATE auth_refresh_tokens SET session_id = ? WHERE session_id = ?")
        .bind(&moved_sid)
        .bind(&signed_sid)
        .execute(pool.get())
        .await
        .unwrap();
    assert_eq!(moved.rows_affected(), 1);
    let mut drifted_headers = axum::http::HeaderMap::new();
    drifted_headers.insert(
        "authorization",
        format!("Bearer {}", drifted.access_token).parse().unwrap(),
    );
    assert_eq!(
        second
            .current_principal(&drifted_headers)
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        first
            .create_browser_session(adoption(&drifted_handle, &drifted))
            .await
            .err()
            .unwrap()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        first
            .refresh(refresh(&drifted.refresh_token))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    let restored =
        sqlx::query("UPDATE auth_refresh_tokens SET session_id = ? WHERE session_id = ?")
            .bind(&signed_sid)
            .bind(&moved_sid)
            .execute(pool.get())
            .await
            .unwrap();
    assert_eq!(restored.rows_affected(), 1);
    assert!(first.refresh(refresh(&drifted.refresh_token)).await.is_ok());

    // Copied hashes cannot make a signed JWT authorize another session or user.
    let collision_name = register(&first).await;
    let first_token = login(&first, &collision_name).await;
    let second_token = login(&first, &collision_name).await;
    let third_name = register(&first).await;
    let third_token = login(&first, &third_name).await;
    let mut others = Vec::new();
    for other in [&second_token, &third_token] {
        let hint = handle(other);
        let sid = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(hint.split('.').nth(3).unwrap())
                .unwrap(),
        )
        .unwrap();
        let changed =
            sqlx::query("UPDATE auth_refresh_tokens SET token_hash = ? WHERE session_id = ?")
                .bind(digest(&first_token.refresh_token))
                .bind(&sid)
                .execute(pool.get())
                .await
                .unwrap();
        assert_eq!(changed.rows_affected(), 1);
        others.push((other, sid));
    }
    assert!(
        first
            .refresh(refresh(&first_token.refresh_token))
            .await
            .is_ok()
    );
    assert_eq!(
        first
            .refresh(refresh(&first_token.refresh_token))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    for (other, sid) in others {
        let (stored_hash, revoked): (String, i16) = sqlx::query_as(
            "SELECT token_hash, is_revoked FROM auth_refresh_tokens WHERE session_id = ?",
        )
        .bind(&sid)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(stored_hash, digest(&first_token.refresh_token));
        assert_eq!(revoked, 0);
        assert_eq!(
            first
                .refresh(refresh(&other.refresh_token))
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        let restored =
            sqlx::query("UPDATE auth_refresh_tokens SET token_hash = ? WHERE session_id = ?")
                .bind(digest(&other.refresh_token))
                .bind(&sid)
                .execute(pool.get())
                .await
                .unwrap();
        assert_eq!(restored.rows_affected(), 1);
        assert!(first.refresh(refresh(&other.refresh_token)).await.is_ok());
    }

    let tokens = login(&first, &name).await;
    let browser = handle(&tokens);
    first
        .create_browser_session(adoption(&browser, &tokens))
        .await
        .unwrap();
    // Creation recovery uses the independent proof, even if acknowledgement was
    // lost and the input JWT has already been consumed.
    second
        .create_browser_session(adoption(&browser, &tokens))
        .await
        .unwrap();
    assert_eq!(
        first
            .refresh(refresh(&tokens.refresh_token))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    let current = first.refresh(refresh(&browser)).await.unwrap();
    assert_eq!(current.refresh_token, browser);
    let unchanged = second.refresh(refresh(&browser)).await.unwrap();
    assert_eq!(
        current.access_token, unchanged.access_token,
        "normal reads must not rotate/write"
    );

    // The complete handle binds both runtime and proof, including if an owner
    // row is misplaced. Restoring it must leave canonical authority unchanged.
    let proof = browser.split('.').skip(2).collect::<Vec<_>>().join(".");
    let other_runtime = format!(
        "bs3.{}.{proof}",
        URL_SAFE_NO_PAD.encode("http://localhost:17002/")
    );
    assert_eq!(
        first.refresh(refresh(&other_runtime)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let moved = sqlx::query(
        "UPDATE auth_refresh_tokens SET browser_handle_hash = ? WHERE browser_handle_hash = ?",
    )
    .bind(digest(&other_runtime))
    .bind(digest(&browser))
    .execute(pool.get())
    .await
    .unwrap();
    assert_eq!(moved.rows_affected(), 1);
    assert_eq!(
        first.refresh(refresh(&other_runtime)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let restored = sqlx::query(
        "UPDATE auth_refresh_tokens SET browser_handle_hash = ? WHERE browser_handle_hash = ?",
    )
    .bind(digest(&browser))
    .bind(digest(&other_runtime))
    .execute(pool.get())
    .await
    .unwrap();
    assert_eq!(restored.rows_affected(), 1);
    assert!(first.refresh(refresh(&browser)).await.is_ok());

    // Trace the real ordinary owner resolution, including all SQLx calls.
    // Each independent request must remain one read and no transaction/write.
    {
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::prelude::*;
        #[derive(Clone)]
        struct Queries(Arc<std::sync::Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Queries {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target() != "sqlx::query" {
                    return;
                }
                struct Summary(String);
                impl tracing::field::Visit for Summary {
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        if field.name() == "summary" {
                            self.0 = value.to_owned();
                        }
                    }
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "summary" {
                            self.0 = format!("{value:?}").trim_matches('"').to_owned();
                        }
                    }
                }
                let mut summary = Summary(String::new());
                event.record(&mut summary);
                self.0.lock().unwrap().push(summary.0);
            }
        }
        let queries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber =
            tracing::Dispatch::new(tracing_subscriber::registry().with(Queries(queries.clone())));
        let mut times = Vec::new();
        for _ in 0..40 {
            let start = std::time::Instant::now();
            first
                .refresh(refresh(&browser))
                .with_subscriber(subscriber.clone())
                .await
                .unwrap();
            times.push(start.elapsed().as_micros());
        }
        times.sort_unstable();
        let queries = queries.lock().unwrap();
        let reads = queries
            .iter()
            .filter(|sql| sql.starts_with("SELECT"))
            .count();
        let initializations = queries
            .iter()
            .filter(|sql| sql.starts_with("SET sql_mode="))
            .count();
        assert_eq!(reads, 40, "{queries:?}");
        assert!(
            queries
                .iter()
                .all(|sql| sql.starts_with("SELECT") || sql.starts_with("SET sql_mode=")),
            "{queries:?}"
        );
        eprintln!(
            "ordinary browser resolution: 40 requests, 40 business SELECTs, zero session writes/transactions, {initializations} pool initialization SETs; p50={}us p95={}us",
            times[20], times[38]
        );
    }

    let old_jwt = expire_access(&pool, &browser).await;
    let start = Arc::new(tokio::sync::Barrier::new(9));
    let mut requests = Vec::new();
    for n in 0..8 {
        let service = if n % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let value = browser.clone();
        let start = start.clone();
        requests.push(tokio::spawn(async move {
            start.wait().await;
            service.refresh(refresh(&value)).await
        }));
    }
    start.wait().await;
    let mut issued = Vec::new();
    for request in requests {
        issued.push(request.await.unwrap().unwrap());
    }
    let delayed = second.refresh(refresh(&browser)).await.unwrap();
    for result in issued {
        assert_eq!(result.access_token, delayed.access_token);
        assert_eq!(result.user_id, tokens.user_id);
    }
    assert_eq!(
        first.refresh(refresh(&old_jwt)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE user_id = ?")
            .bind(&tokens.user_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(count, 1, "rotation replaces one canonical row");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(&browser))
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(count, 1);

    let mut bounded_settings = settings.clone();
    bounded_settings.db_pool_max_connections = 1;
    bounded_settings.db_pool_min_connections = 1;
    bounded_settings.db_pool_acquire_timeout_secs = 1;
    let bounded_pool = SharedPool::new(&bounded_settings).await.unwrap();
    let bounded = make_service(bounded_pool.clone());
    let blocked_connection = bounded_pool.get().acquire().await.unwrap();
    let direct = login(&first, &name).await;
    let error = bounded
        .refresh(refresh(&direct.refresh_token))
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.1.0.error_code.as_deref(), Some("database_error"));
    drop(blocked_connection);
    bounded
        .refresh(refresh(&direct.refresh_token))
        .await
        .unwrap();
    // Independent logins preserve the old session until explicit logout.
    let other_session = login(&first, &name).await;
    let other_handle = handle(&other_session);
    first
        .create_browser_session(adoption(&other_handle, &other_session))
        .await
        .unwrap();
    let name_b = register(&first).await;
    let tokens_b = login(&second, &name_b).await;
    let new_handle = handle(&tokens_b);
    second
        .create_browser_session(adoption(&new_handle, &tokens_b))
        .await
        .unwrap();
    // Duplicate stored hashes must not redirect lookup or logout to another session.
    let changed = sqlx::query(
        "UPDATE auth_refresh_tokens SET browser_handle_hash = ? WHERE browser_handle_hash = ?",
    )
    .bind(digest(&browser))
    .bind(digest(&new_handle))
    .execute(pool.get())
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    assert_eq!(
        first.refresh(refresh(&browser)).await.unwrap().user_id,
        tokens.user_id
    );
    let a_parts: Vec<_> = browser.split('.').collect();
    let b_parts: Vec<_> = new_handle.split('.').collect();
    let forged = format!(
        "bs3.{}.{}.{}.{}",
        b_parts[1], b_parts[2], b_parts[3], a_parts[4]
    );
    assert_eq!(
        first.refresh(refresh(&forged)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    first.logout(refresh(&browser)).await.unwrap();
    let restored = sqlx::query("UPDATE auth_refresh_tokens SET browser_handle_hash = ? WHERE browser_handle_hash = ? AND user_id = ? AND is_revoked = 0")
        .bind(digest(&new_handle)).bind(digest(&browser)).bind(&tokens_b.user_id)
        .execute(pool.get()).await.unwrap();
    assert_eq!(
        restored.rows_affected(),
        1,
        "logout must not revoke the other session"
    );
    assert_eq!(
        second.refresh(refresh(&new_handle)).await.unwrap().user_id,
        tokens_b.user_id
    );
    // A deleted owner must not let a different account reclaim its old handle.
    let attacker = login(&second, &name_b).await;
    assert!(matches!(
        first
            .create_browser_session(adoption(&browser, &attacker))
            .await,
        Err((StatusCode::UNAUTHORIZED, _))
    ));
    assert_eq!(
        first.refresh(refresh(&browser)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    second
        .refresh(refresh(&attacker.refresh_token))
        .await
        .expect("failed adoption must not consume B's refresh token");
    assert_eq!(
        first.refresh(refresh(&new_handle)).await.unwrap().user_id,
        tokens_b.user_id
    );
    assert_eq!(
        second
            .refresh(refresh(&other_handle))
            .await
            .unwrap()
            .user_id,
        tokens.user_id
    );

    // Refresh-before-logout revokes its successor; logout-before-refresh denies.
    expire_access(&pool, &new_handle).await;
    first.refresh(refresh(&new_handle)).await.unwrap();
    second.logout(refresh(&new_handle)).await.unwrap();
    assert_eq!(
        first.refresh(refresh(&new_handle)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    first.logout(refresh(&new_handle)).await.unwrap();
    second.logout(refresh(&other_handle)).await.unwrap();
    assert_eq!(
        first.refresh(refresh(&other_handle)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );

    // A second handle cannot replace an already-owned canonical session.
    // Its compare-and-swap must leave the existing credential intact.
    let blocker = login(&first, &name).await;
    let blocked = handle(&blocker);
    first
        .create_browser_session(adoption(&blocked, &blocker))
        .await
        .unwrap();
    let encrypted: String = sqlx::query_scalar(
        "SELECT browser_ciphertext FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(&blocked))
    .fetch_one(pool.get())
    .await
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(
        &FernetTokenEncryptor::new(ENCRYPTION_KEY)
            .unwrap()
            .decrypt(&encrypted)
            .unwrap(),
    )
    .unwrap();
    let rollback_tokens = astra_services::AuthTokenRecord {
        user_id: blocker.user_id.clone(),
        access_token: payload["access_token"].as_str().unwrap().to_owned(),
        refresh_token: payload["refresh_token"].as_str().unwrap().to_owned(),
        token_type: "bearer".into(),
        expires_in: 300,
    };
    let duplicate = handle(&rollback_tokens);
    assert!(
        matches!(
            first
                .create_browser_session(adoption(&duplicate, &rollback_tokens))
                .await,
            Err((StatusCode::UNAUTHORIZED, _))
        ),
        "an owned session cannot be adopted by another handle"
    );
    assert_eq!(
        first.refresh(refresh(&blocked)).await.unwrap().user_id,
        blocker.user_id
    );
    first
        .refresh(refresh(&rollback_tokens.refresh_token))
        .await
        .unwrap();

    let key_tokens = login(&first, &name).await;
    let keyed = handle(&key_tokens);
    first
        .create_browser_session(adoption(&keyed, &key_tokens))
        .await
        .unwrap();
    let wrong_key = DatabaseAuthService::new(settings.clone(), jwt)
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new("wrong-key").unwrap());
    assert_eq!(
        wrong_key.refresh(refresh(&keyed)).await.unwrap_err().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(first.refresh(refresh(&keyed)).await.is_ok());
    sqlx::query("UPDATE auth_users SET is_active = 0 WHERE user_id = ?")
        .bind(&tokens.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert_eq!(
        second.refresh(refresh(&keyed)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    first.logout(refresh(&keyed)).await.unwrap();
    sqlx::query("UPDATE auth_users SET is_active = 1 WHERE user_id = ?")
        .bind(&tokens.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert_eq!(
        second.refresh(refresh(&keyed)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED,
        "logout while inactive must remain revoked after reactivation"
    );
    let expiry_tokens = login(&first, &name).await;
    let keyed = handle(&expiry_tokens);
    first
        .create_browser_session(adoption(&keyed, &expiry_tokens))
        .await
        .unwrap();
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = '2020-01-01', created_at = '2020-01-01' WHERE browser_handle_hash = ?")
        .bind(digest(&keyed))
        .execute(pool.get())
        .await
        .unwrap();
    assert_eq!(
        first.refresh(refresh(&keyed)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let cleanup = astra_services::cleanup_expired_data(
        pool.get(),
        &astra_services::RetentionPolicy::default(),
    )
    .await
    .unwrap();
    assert!(
        cleanup
            .iter()
            .find(|r| r.table == "auth_refresh_tokens")
            .unwrap()
            .rows_deleted
            >= 1
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(&keyed))
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        remaining, 0,
        "cleanup must remove the expired session's owner"
    );
    for user in [&tokens.user_id, &tokens_b.user_id] {
        sqlx::query("DELETE FROM auth_refresh_tokens WHERE user_id = ?")
            .bind(user)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ?")
            .bind(user)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
            .bind(user)
            .execute(pool.get())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn provider_verification_holds_no_account_lock_and_cannot_publish_a_stale_binding() {
    use axum::{Json, Router, http::HeaderMap, routing::get};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (pool, settings) = common::setup_pool_and_settings().await;
    let hold_all = Arc::new(AtomicBool::new(false));
    let contenders = Arc::new(tokio::sync::Barrier::new(9));
    let hold_next = Arc::new(AtomicBool::new(false));
    let reject_held = Arc::new(AtomicBool::new(false));
    let reject_next = Arc::new(AtomicBool::new(false));
    let unavailable = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let subject = uuid::Uuid::new_v4().to_string();
    let app = Router::new().route("/auth/whoami", get({
        let hold_all = hold_all.clone(); let contenders = contenders.clone();
        let hold = hold_next.clone(); let entered = entered.clone(); let release = release.clone(); let unavailable = unavailable.clone();
        let reject_held = reject_held.clone(); let reject_next = reject_next.clone();
        move |headers: HeaderMap| {
            let synchronize = hold_all.load(Ordering::SeqCst); let contenders = contenders.clone();
            let hold = hold.swap(false, Ordering::SeqCst); let entered = entered.clone(); let release = release.clone();
            let rejected = reject_next.swap(false, Ordering::SeqCst); let reject_held = reject_held.clone();
            let subject = subject.clone(); let unavailable = unavailable.load(Ordering::SeqCst);
            async move {
                let key = headers["authorization"].to_str().unwrap().trim_start_matches("Bearer ").to_owned();
                if hold { entered.notify_one(); release.notified().await; }
                if synchronize { contenders.wait().await; }
                (if unavailable { StatusCode::SERVICE_UNAVAILABLE } else if rejected || (hold && reject_held.swap(false, Ordering::SeqCst)) { StatusCode::UNAUTHORIZED } else { StatusCode::OK }, Json(serde_json::json!({
                    "user_id":subject, "key_id":key, "is_active":true, "is_master":false,
                    "scope":{"type":"personal","id":subject}, "api_version":"1",
                    "capabilities":["api_key_scopes","memory_filters_v1"], "granted_scopes":["identity:read"]
                })))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let auth = Arc::new(
        DatabaseAuthService::new(
            settings,
            JwtSettings {
                secret_key: JWT_KEY.into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 90,
                refresh_token_expire_days: 1,
            },
        )
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new(ENCRYPTION_KEY).unwrap())
        .with_memoria_base_url(url),
    );
    let tokens = auth.login_memoria("first-key").await.unwrap().tokens;
    let browser = handle(&tokens);
    auth.create_browser_session(adoption(&browser, &tokens))
        .await
        .unwrap();
    // Model an internal session issued before its account was provider-bound.
    fixture::age_access(&pool, &browser, 1200, 3600, Some("internal")).await;
    let resolved = auth.refresh(refresh(&browser)).await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", resolved.access_token).parse().unwrap(),
    );
    auth.current_user(&headers).await.unwrap();
    assert!(resolved.expires_in <= 900);
    expire_access(&pool, &browser).await;
    hold_all.store(true, Ordering::SeqCst);
    let replica = Arc::new(
        auth.as_ref()
            .clone()
            .with_pool(SharedPool::new(pool.settings()).await.unwrap()),
    );
    let mut requests = Vec::new();
    for index in 0..8 {
        let auth = if index % 2 == 0 {
            auth.clone()
        } else {
            replica.clone()
        };
        let browser = browser.clone();
        requests.push(tokio::spawn(async move {
            auth.refresh(refresh(&browser)).await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), contenders.wait())
        .await
        .unwrap();
    hold_all.store(false, Ordering::SeqCst);
    let mut issued = Vec::new();
    for request in requests {
        issued.push(request.await.unwrap().unwrap().access_token);
    }
    assert!(
        issued.iter().all(|token| token == &issued[0]),
        "all eight verified the same old pair before any could commit"
    );
    expire_access(&pool, &browser).await;
    hold_next.store(true, Ordering::SeqCst);
    let stale = tokio::spawn({
        let auth = auth.clone();
        let browser = browser.clone();
        async move { auth.refresh(refresh(&browser)).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    // Real login takes the canonical account lock. It must finish while the
    // refresh provider request is still blocked, and replace the binding.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        auth.login_memoria("replacement-key"),
    )
    .await
    .unwrap()
    .unwrap();
    release.notify_one();
    stale
        .await
        .unwrap()
        .expect("binding replacement is reverified");
    assert!(
        auth.refresh(refresh(&browser)).await.is_ok(),
        "fresh verification of the replacement binding recovers"
    );

    // The direct single-use refresh path must reverify the new binding too,
    // without accepting a replay of the consumed refresh token.
    let direct = auth.login_memoria("replacement-key").await.unwrap().tokens;
    reject_held.store(true, Ordering::SeqCst);
    hold_next.store(true, Ordering::SeqCst);
    let pending_direct = tokio::spawn({
        let auth = auth.clone();
        let token = direct.refresh_token.clone();
        async move { auth.refresh(refresh(&token)).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    auth.login_memoria("third-key").await.unwrap();
    release.notify_one();
    pending_direct
        .await
        .unwrap()
        .expect("direct refresh reverified");
    assert_eq!(
        auth.refresh(refresh(&direct.refresh_token))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );

    expire_access(&pool, &browser).await;
    reject_next.store(true, Ordering::SeqCst);
    assert_eq!(
        auth.refresh(refresh(&browser)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED,
        "an unchanged binding rejected by the provider must not be retried"
    );
    unavailable.store(true, Ordering::SeqCst);
    let before: String = sqlx::query_scalar(
        "SELECT browser_ciphertext FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(&browser))
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        auth.refresh(refresh(&browser)).await.unwrap_err().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let after: String = sqlx::query_scalar(
        "SELECT browser_ciphertext FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(&browser))
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        before, after,
        "provider outage must not publish credentials"
    );
    unavailable.store(false, Ordering::SeqCst);

    hold_next.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let auth = auth.clone();
        let browser = browser.clone();
        async move { auth.refresh(refresh(&browser)).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        auth.disconnect_memoria(&tokens.user_id),
    )
    .await
    .unwrap()
    .unwrap();
    release.notify_one();
    assert_eq!(
        pending.await.unwrap().unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    auth.login_memoria("replacement-key").await.unwrap();
    assert_eq!(
        auth.refresh(refresh(&browser)).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED,
        "reconnect must not resurrect the old session"
    );
    server.abort();
    sqlx::query("UPDATE auth_refresh_tokens SET browser_handle_hash = NULL, browser_ciphertext = NULL WHERE browser_handle_hash = ?")
        .bind(digest(&browser))
        .execute(pool.get())
        .await
        .unwrap();
    for table in [
        "auth_tokens",
        "auth_external_identities",
        "auth_refresh_tokens",
        "auth_user_roles",
        "auth_users",
    ] {
        let column = match table {
            "auth_tokens" => "scope_user_id",
            "auth_external_identities" => "astra_user_id",
            _ => "user_id",
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {column} = ?"
        )))
        .bind(&tokens.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn fresh_login_and_logout_do_not_depend_on_expired_credentials_or_an_old_encryption_key() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let service = |key: &str| {
        DatabaseAuthService::new(
            settings.clone(),
            JwtSettings {
                secret_key: JWT_KEY.into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 5,
                refresh_token_expire_days: 1,
            },
        )
        .with_pool(pool.clone())
        .with_encryptor(FernetTokenEncryptor::new(key).unwrap())
    };
    let original = service(ENCRYPTION_KEY);
    for scenario in ["expired", "key_replaced", "account_deleted"] {
        let name = register(&original).await;
        let tokens = login(&original, &name).await;
        let old = handle(&tokens);
        original
            .create_browser_session(adoption(&old, &tokens))
            .await
            .unwrap();
        if scenario == "expired" {
            sqlx::query(
                "UPDATE auth_refresh_tokens SET expires_at = '2020-01-01' WHERE user_id = ?",
            )
            .bind(&tokens.user_id)
            .execute(pool.get())
            .await
            .unwrap();
        }
        if scenario == "account_deleted" {
            sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
                .bind(&tokens.user_id)
                .execute(pool.get())
                .await
                .unwrap();
        }
        let current = service(if scenario == "key_replaced" {
            "replacement-key"
        } else {
            ENCRYPTION_KEY
        });
        assert!(current.refresh(refresh(&old)).await.is_err());
        let next_name = register(&current).await;
        let next = login(&current, &next_name).await;
        let new_handle = handle(&next);
        current
            .create_browser_session(adoption(&new_handle, &next))
            .await
            .unwrap();
        assert_eq!(
            current.refresh(refresh(&new_handle)).await.unwrap().user_id,
            next.user_id
        );
        // Logout is independent of decryption and account activity.
        current.logout(refresh(&old)).await.unwrap();
        assert_eq!(
            original.refresh(refresh(&old)).await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        // Revocation remains possible even if this replica cannot decrypt the
        // delivery record. It never returns credentials or grants access.
        original.logout(refresh(&new_handle)).await.unwrap();
        original.logout(refresh(&new_handle)).await.unwrap();
        assert_eq!(
            current.refresh(refresh(&new_handle)).await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        for user in [&tokens.user_id, &next.user_id] {
            for table in ["auth_refresh_tokens", "auth_user_roles", "auth_users"] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE user_id = ?"
                )))
                .bind(user)
                .execute(pool.get())
                .await
                .unwrap();
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn revocation_shortens_retention_without_extending_expired_sessions() {
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let settings = isolated.settings.clone();
    let pool = isolated.pool.clone();
    isolated
        .run(async move {
            let auth = DatabaseAuthService::new(
                settings,
                JwtSettings {
                    secret_key: JWT_KEY.into(),
                    algorithm: "HS256".into(),
                    access_token_expire_minutes: 5,
                    refresh_token_expire_days: 30,
                },
            )
            .with_pool(pool.clone())
            .with_encryptor(FernetTokenEncryptor::new(ENCRYPTION_KEY).unwrap());
            let name = register(&auth).await;
            let other_name = register(&auth).await;
            let direct = login(&auth, &name).await;
            let browser = login(&auth, &name).await;
            let other = login(&auth, &other_name).await;
            let proof = handle(&browser);
            auth.create_browser_session(adoption(&proof, &browser))
                .await
                .unwrap();
            auth.logout(refresh(&direct.refresh_token)).await.unwrap();
            auth.logout(refresh(&proof)).await.unwrap();
            let rows: Vec<(String, chrono::NaiveDateTime)> = sqlx::query_as(
                "SELECT session_id, expires_at FROM auth_refresh_tokens WHERE user_id = ?",
            )
            .bind(&direct.user_id)
            .fetch_all(pool.get())
            .await
            .unwrap();
            assert_eq!(rows.len(), 2);
            let now: chrono::NaiveDateTime = sqlx::query_scalar("SELECT UTC_TIMESTAMP(6)")
                .fetch_one(pool.get())
                .await
                .unwrap();
            assert!(rows.iter().all(|(_, expiry)| *expiry <= now));
            let active = login(&auth, &name).await;
            let active_proof = handle(&active);
            auth.create_browser_session(adoption(&active_proof, &active)).await.unwrap();
            auth.disconnect_memoria(&direct.user_id).await.unwrap();
            let retired: (chrono::NaiveDateTime, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT expires_at, browser_handle_hash, browser_ciphertext FROM auth_refresh_tokens WHERE user_id = ? AND session_id NOT IN (?, ?)"
            ).bind(&direct.user_id).bind(&rows[0].0).bind(&rows[1].0).fetch_one(pool.get()).await.unwrap();
            let revoked_at: chrono::NaiveDateTime = sqlx::query_scalar("SELECT UTC_TIMESTAMP(6)").fetch_one(pool.get()).await.unwrap();
            assert!(retired.0 <= revoked_at);
            assert!(retired.1.is_none() && retired.2.is_none());
            assert_eq!(auth.refresh(refresh(&active_proof)).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
            for (id, expiry) in &rows {
                let repeated: chrono::NaiveDateTime = sqlx::query_scalar(
                    "SELECT expires_at FROM auth_refresh_tokens WHERE session_id = ?",
                )
                .bind(id)
                .fetch_one(pool.get())
                .await
                .unwrap();
                assert_eq!(
                    *expiry, repeated,
                    "repeated revocation must not postpone cleanup"
                );
            }
            let recent = astra_services::cleanup_refresh_session_batch(
                pool.get(),
                &astra_services::RetentionPolicy::default(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                recent.deleted, 0,
                "newly revoked rows retain the configured grace period"
            );
            sqlx::query(
                "UPDATE auth_refresh_tokens SET expires_at = '2020-01-01' WHERE user_id = ?",
            )
            .bind(&direct.user_id)
            .execute(pool.get())
            .await
            .unwrap();
            auth.disconnect_memoria(&direct.user_id).await.unwrap();
            let old = astra_services::cleanup_refresh_session_batch(
                pool.get(),
                &astra_services::RetentionPolicy::default(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(old.deleted, 3, "disconnect must preserve an earlier expiry");
            auth.logout(refresh(&direct.refresh_token)).await.unwrap();
            auth.logout(refresh(&proof)).await.unwrap();
            assert!(
                auth.refresh(refresh(&other.refresh_token)).await.is_ok(),
                "another account remains usable"
            );
            assert!(auth.refresh(refresh(&proof)).await.is_err());
        })
        .await;
}
