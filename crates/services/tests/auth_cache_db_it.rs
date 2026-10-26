mod common;

use astra_core::JwtSettings;
use astra_services::{
    AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    DatabaseAuthService,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tracing_subscriber::prelude::*;

#[derive(Clone)]
struct Queries {
    count: Arc<AtomicUsize>,
    snapshot: Arc<SnapshotBarrier>,
}

#[derive(Default)]
struct SnapshotBarrier {
    armed: AtomicBool,
    observed: tokio::sync::Notify,
    released: Mutex<bool>,
    release: Condvar,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Queries {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.count.fetch_add(1, Ordering::SeqCst);
            #[derive(Default)]
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{value:?}"));
                }
            }
            let mut fields = Fields::default();
            event.record(&mut fields);
            if fields.0.contains("SELECT u.user_id, u.username")
                && self.snapshot.armed.swap(false, Ordering::SeqCst)
            {
                self.snapshot.observed.notify_one();
                // The SQL result is ready; let logout finish on another worker
                // without blocking the runtime's I/O driver.
                tokio::task::block_in_place(|| {
                    let (released, timeout) = self
                        .snapshot
                        .release
                        .wait_timeout_while(
                            self.snapshot.released.lock().unwrap(),
                            std::time::Duration::from_secs(20),
                            |released| !*released,
                        )
                        .unwrap();
                    assert!(
                        *released && !timeout.timed_out(),
                        "authentication snapshot barrier timed out"
                    );
                });
            }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn jwt_authentication_uses_one_cold_query_and_zero_warm_queries_with_local_revocation() {
    let count = Arc::new(AtomicUsize::new(0));
    let snapshot = Arc::new(SnapshotBarrier::default());
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Queries {
        count: count.clone(),
        snapshot: snapshot.clone(),
    }))
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
    sqlx::query("UPDATE auth_users SET is_active = 1 WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    let other_name = format!("cache_other_{}", uuid::Uuid::new_v4().simple());
    let other = service
        .register(AuthRegisterRequestData {
            username: other_name.clone(),
            email: format!("{other_name}@example.test"),
            password: password.into(),
            display_name: None,
        })
        .await
        .unwrap();
    let other_tokens = service
        .login(AuthLoginRequestData {
            username: other_name,
            password: password.into(),
        })
        .await
        .unwrap();
    for (refresh, same_session) in [
        (&other_tokens.refresh_token, false),
        (&tokens[1].refresh_token, true),
    ] {
        let observer = service.clone().with_pool(pool.clone());
        *snapshot.released.lock().unwrap() = false;
        snapshot.armed.store(true, Ordering::SeqCst);
        let request_auth = observer.clone();
        let request_headers = second.clone();
        let pending =
            tokio::spawn(async move { request_auth.current_principal(&request_headers).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            snapshot.observed.notified(),
        )
        .await
        .unwrap();
        let logout = observer
            .logout(AuthRefreshRequestData {
                refresh_token: refresh.clone(),
            })
            .await;
        *snapshot.released.lock().unwrap() = true;
        snapshot.release.notify_all();
        logout.unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap().user.user_id,
            user.user_id,
            "a successful SQL snapshot authenticates an overlapping request"
        );
        count.store(0, Ordering::SeqCst);
        let subsequent = observer.current_principal(&second).await;
        assert_eq!(
            count.swap(0, Ordering::SeqCst),
            1,
            "invalidated fill cannot re-enter cache"
        );
        if same_session {
            assert_eq!(
                subsequent.unwrap_err().0,
                StatusCode::UNAUTHORIZED,
                "a request starting after its logout must fail"
            );
        } else {
            subsequent.expect("another user's logout cannot revoke this principal");
            observer.current_principal(&second).await.unwrap();
            assert_eq!(
                count.swap(0, Ordering::SeqCst),
                0,
                "fresh authentication can be retained again"
            );
        }
    }
    sqlx::query("DELETE FROM auth_refresh_tokens WHERE user_id = ?")
        .bind(&other.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ?")
        .bind(&other.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
        .bind(&other.user_id)
        .execute(pool.get())
        .await
        .unwrap();
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
