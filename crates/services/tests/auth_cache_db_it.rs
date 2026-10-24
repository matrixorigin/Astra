mod common;

use astra_core::JwtSettings;
use astra_services::{
    AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    DatabaseAuthService,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing_subscriber::prelude::*;

#[derive(Clone)]
struct Queries(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Queries {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

#[tokio::test]
#[ignore = "requires live MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn jwt_authentication_uses_one_cold_query_and_zero_warm_queries_with_local_revocation() {
    let count = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Queries(count.clone())),
    )
    .expect("this integration binary owns its SQL counter");
    let (pool, settings) = common::setup_pool_and_settings().await;
    let service = DatabaseAuthService::new(
        settings,
        JwtSettings {
            secret_key: "isolated-auth-cache-test-key".into(),
            algorithm: "HS256".into(),
            access_token_expire_minutes: 5,
            refresh_token_expire_days: 1,
        },
    )
    .with_pool(pool.clone());
    let username = format!("cache_{}", uuid::Uuid::new_v4().simple());
    let password = "correct-horse-battery-staple";
    let user = service
        .register(AuthRegisterRequestData {
            username: username.clone(),
            email: format!("{username}@example.test"),
            password: password.into(),
            display_name: None,
        })
        .await
        .unwrap();
    let mut tokens = Vec::new();
    for _ in 0..2 {
        tokens.push(
            service
                .login(AuthLoginRequestData {
                    username: username.clone(),
                    password: password.into(),
                })
                .await
                .unwrap(),
        );
    }
    let first = headers(&tokens[0].access_token);
    let second = headers(&tokens[1].access_token);
    count.store(0, Ordering::SeqCst);
    let principal = service.current_principal(&first).await.unwrap();
    assert_eq!(principal.user.user_id, user.user_id);
    assert_eq!(
        count.swap(0, Ordering::SeqCst),
        1,
        "cold authentication is one snapshot"
    );
    for _ in 0..3 {
        assert_eq!(
            service.clone().current_principal(&first).await.unwrap(),
            principal
        );
    }
    assert_eq!(
        count.swap(0, Ordering::SeqCst),
        0,
        "warm requests and clones perform no SQL"
    );
    let second_principal = service.current_principal(&second).await.unwrap();
    assert_ne!(second_principal.session_id, principal.session_id);
    assert_eq!(
        count.swap(0, Ordering::SeqCst),
        1,
        "another session cannot borrow the first cache entry"
    );
    assert_eq!(
        service
            .current_principal(&headers("invalid.token"))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        count.swap(0, Ordering::SeqCst),
        0,
        "invalid JWTs never reach the database"
    );

    service
        .logout(AuthRefreshRequestData {
            refresh_token: tokens[0].refresh_token.clone(),
        })
        .await
        .unwrap();
    count.store(0, Ordering::SeqCst);
    let error = service.clone().current_principal(&first).await.unwrap_err();
    assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    assert_eq!(error.1.0.detail, "Session expired or revoked");
    assert_eq!(count.swap(0, Ordering::SeqCst), 1);
    service
        .current_principal(&second)
        .await
        .expect("logout does not revoke another device");

    sqlx::query("UPDATE auth_users SET is_active = 0 WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    service
        .current_principal(&second)
        .await
        .expect("out-of-band changes have a bounded freshness delay");
    let fresh = service.clone().with_pool(pool.clone());
    assert_eq!(
        fresh.current_principal(&second).await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("DELETE FROM auth_refresh_tokens WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    pool.get().close().await;
}
