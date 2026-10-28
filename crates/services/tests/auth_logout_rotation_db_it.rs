mod common;

use astra_core::JwtSettings;
use astra_services::{
    AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    DatabaseAuthService, RetentionPolicy, cleanup_expired_data,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tracing_subscriber::prelude::*;

#[derive(Default)]
struct AccountLockGate {
    armed: AtomicBool,
    acquired: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

impl AccountLockGate {
    fn arm(&self) {
        *self.released.lock().unwrap() = false;
        self.armed.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct PauseAccountLock(Arc<AccountLockGate>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PauseAccountLock {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" || !self.0.armed.load(Ordering::SeqCst) {
            return;
        }
        #[derive(Default)]
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields
            .0
            .contains("FROM auth_users WHERE user_id = ? FOR UPDATE")
            && self.0.armed.swap(false, Ordering::SeqCst)
        {
            self.0.acquired.notify_one();
            tokio::task::block_in_place(|| {
                let (released, timeout) = self
                    .0
                    .wake
                    .wait_timeout_while(
                        self.0.released.lock().unwrap(),
                        std::time::Duration::from_secs(20),
                        |released| !*released,
                    )
                    .unwrap();
                assert!(
                    *released && !timeout.timed_out(),
                    "account lock barrier timed out"
                );
            });
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
async fn logout_revokes_the_session_across_refresh_rotation_without_revoking_other_sessions() {
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let pool = isolated.pool.clone();
    let settings = isolated.settings.clone();
    isolated
        .run(
            logout_revokes_the_session_across_refresh_rotation_without_revoking_other_sessions_body(
                pool, settings,
            ),
        )
        .await;
}

async fn logout_revokes_the_session_across_refresh_rotation_without_revoking_other_sessions_body(
    pool: astra_core::SharedPool,
    settings: astra_core::MatrixOneSettings,
) {
    let gate = Arc::new(AccountLockGate::default());
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(PauseAccountLock(gate.clone())),
    )
    .expect("this test binary owns its tracing subscriber");
    let service = DatabaseAuthService::new(
        settings,
        JwtSettings {
            secret_key: "isolated-logout-rotation-test-key".into(),
            algorithm: "HS256".into(),
            access_token_expire_minutes: 5,
            refresh_token_expire_days: 1,
        },
    )
    .with_pool(pool.clone());
    let username = format!("logout_{}", uuid::Uuid::new_v4().simple());
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
    let login = || AuthLoginRequestData {
        username: username.clone(),
        password: password.into(),
    };
    let original = service.login(login()).await.unwrap();
    // Model a still-valid token issued more than the cleanup retention age ago.
    sqlx::query("UPDATE auth_refresh_tokens SET created_at = DATE_SUB(NOW(6), INTERVAL 8 DAY) WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    let other = service.login(login()).await.unwrap();
    let key = b"isolated-logout-rotation-test-key";
    let mut expired_claims = jsonwebtoken::decode::<serde_json::Value>(
        &original.refresh_token,
        &jsonwebtoken::DecodingKey::from_secret(key),
        &jsonwebtoken::Validation::default(),
    )
    .unwrap()
    .claims;
    // This is inside the JWT library's usual expiry leeway.
    expired_claims["exp"] = (chrono::Utc::now().timestamp() - 1).into();
    let expired = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &expired_claims,
        &jsonwebtoken::EncodingKey::from_secret(key),
    )
    .unwrap();
    for invalid in [
        original.access_token.clone(),
        "invalid.token".to_string(),
        expired,
    ] {
        assert_eq!(
            service
                .logout(AuthRefreshRequestData {
                    refresh_token: invalid,
                })
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut rotated = service
        .refresh(AuthRefreshRequestData {
            refresh_token: original.refresh_token.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .refresh(AuthRefreshRequestData {
                refresh_token: original.refresh_token.clone()
            })
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Repeated rotations replace the same session row rather than retaining a
    // historical row for every consumed token.
    for _ in 0..3 {
        rotated = service
            .refresh(AuthRefreshRequestData {
                refresh_token: rotated.refresh_token,
            })
            .await
            .unwrap();
    }
    let cleanup = cleanup_expired_data(pool.get(), &RetentionPolicy::default())
        .await
        .unwrap();
    assert!(
        cleanup
            .iter()
            .any(|result| result.table == "auth_refresh_tokens")
    );
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE user_id = ?")
            .bind(&user.user_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(retained, 2, "two sessions retain only one refresh row each");
    service
        .logout(AuthRefreshRequestData {
            refresh_token: original.refresh_token.clone(),
        })
        .await
        .unwrap();
    // An old signed proof still revokes the current row even though its own
    // hash disappeared on rotation. Revoked rows are retained until after expiry.
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = DATE_SUB(UTC_TIMESTAMP(6), INTERVAL 8 DAY), created_at = DATE_SUB(UTC_TIMESTAMP(6), INTERVAL 8 DAY) WHERE user_id = ? AND is_revoked = 1")
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    let cleanup = cleanup_expired_data(pool.get(), &RetentionPolicy::default())
        .await
        .unwrap();
    assert!(
        cleanup
            .iter()
            .any(|result| result.table == "auth_refresh_tokens" && result.rows_deleted > 0)
    );
    assert_eq!(
        service
            .refresh(AuthRefreshRequestData {
                refresh_token: rotated.refresh_token
            })
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        service
            .current_principal(&headers(&rotated.access_token))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    service
        .current_principal(&headers(&other.access_token))
        .await
        .expect("another session remains active");

    let concurrent = service.login(login()).await.unwrap();
    let (left, right) = tokio::join!(
        service.refresh(AuthRefreshRequestData {
            refresh_token: concurrent.refresh_token.clone()
        }),
        service.refresh(AuthRefreshRequestData {
            refresh_token: concurrent.refresh_token.clone()
        }),
    );
    let next = match (left, right) {
        (Ok(next), Err(error)) | (Err(error), Ok(next)) => {
            assert_eq!(error.0, StatusCode::UNAUTHORIZED);
            next
        }
        _ => panic!("a single-use refresh token must have exactly one winner"),
    };
    service
        .logout(AuthRefreshRequestData {
            refresh_token: concurrent.refresh_token,
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .refresh(AuthRefreshRequestData {
                refresh_token: next.refresh_token
            })
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );

    // Pause the winner after it acquires the real account lock so both orders
    // are exercised without depending on task scheduling.
    for refresh_first in [true, false] {
        let token = service.login(login()).await.unwrap();
        gate.arm();
        let winner_service = service.clone();
        let winner_token = token.refresh_token.clone();
        let winner = tokio::spawn(async move {
            if refresh_first {
                winner_service
                    .refresh(AuthRefreshRequestData {
                        refresh_token: winner_token,
                    })
                    .await
                    .map(Some)
            } else {
                winner_service
                    .logout(AuthRefreshRequestData {
                        refresh_token: winner_token,
                    })
                    .await
                    .map(|()| None)
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(15), gate.acquired.notified())
            .await
            .unwrap();
        let other_service = service.clone();
        let other_token = token.refresh_token.clone();
        let mut loser = tokio::spawn(async move {
            if refresh_first {
                other_service
                    .logout(AuthRefreshRequestData {
                        refresh_token: other_token,
                    })
                    .await
                    .map(|()| None)
            } else {
                other_service
                    .refresh(AuthRefreshRequestData {
                        refresh_token: other_token,
                    })
                    .await
                    .map(Some)
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut loser)
                .await
                .is_err()
        );
        gate.release();
        let first = winner.await.unwrap().unwrap();
        let second = loser.await.unwrap();
        if let Some(next) = first {
            second.unwrap();
            assert_eq!(
                service
                    .refresh(AuthRefreshRequestData {
                        refresh_token: next.refresh_token
                    })
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                service
                    .current_principal(&headers(&next.access_token))
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::UNAUTHORIZED
            );
        } else {
            assert_eq!(second.unwrap_err().0, StatusCode::UNAUTHORIZED);
        }
        service
            .current_principal(&headers(&other.access_token))
            .await
            .expect("another session remains active");
    }
    for table in ["auth_refresh_tokens", "auth_user_roles", "auth_users"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user.user_id)
        .execute(pool.get())
        .await
        .unwrap();
    }
}
