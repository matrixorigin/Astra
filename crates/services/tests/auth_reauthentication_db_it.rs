mod common;

use astra_core::JwtSettings;
use astra_services::{
    AuthRegisterRequestData, AuthService, DatabaseAuthService, ReauthenticationPurpose,
    ReauthenticationRequestData,
};
use axum::http::StatusCode;
use serial_test::serial;
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn jwt_settings() -> JwtSettings {
    JwtSettings {
        secret_key: "reauthentication-db-it-secret".to_string(),
        algorithm: "HS256".to_string(),
        access_token_expire_minutes: 5,
        refresh_token_expire_days: 1,
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn reauthentication_proofs_are_owner_purpose_expiry_and_one_time_bound() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let service = DatabaseAuthService::new(settings, jwt_settings()).with_pool(shared_pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("reauth_{suffix}");
    let password = "correct-horse-battery-staple";
    let user = service
        .register(AuthRegisterRequestData {
            username: username.clone(),
            email: format!("{username}@example.test"),
            password: password.to_string(),
            display_name: None,
        })
        .await
        .expect("isolated auth user must register");

    let wrong_password_result = service
        .reauthenticate(
            &user.user_id,
            ReauthenticationRequestData {
                password: "wrong-password".to_string(),
                purpose: ReauthenticationPurpose::DeviceTrust,
            },
        )
        .await;
    let wrong_password = match wrong_password_result {
        Ok(_) => panic!("password knowledge is required"),
        Err(error) => error,
    };
    assert_eq!(wrong_password.0, StatusCode::UNAUTHORIZED);

    let trust = service
        .reauthenticate(
            &user.user_id,
            ReauthenticationRequestData {
                password: password.to_string(),
                purpose: ReauthenticationPurpose::DeviceTrust,
            },
        )
        .await
        .expect("correct password must issue a short-lived proof");
    assert_eq!(trust.purpose, ReauthenticationPurpose::DeviceTrust);
    assert_eq!(trust.expires_in, 300);

    let wrong_purpose = service
        .consume_reauthentication_proof(
            &user.user_id,
            ReauthenticationPurpose::DeviceReenroll,
            &trust.proof,
        )
        .await
        .expect_err("proof purpose cannot be widened or changed");
    assert_eq!(wrong_purpose.0, StatusCode::FORBIDDEN);
    service
        .consume_reauthentication_proof(
            &user.user_id,
            ReauthenticationPurpose::DeviceTrust,
            &trust.proof,
        )
        .await
        .expect("a failed purpose check must not consume the valid authority");
    let replay = service
        .consume_reauthentication_proof(
            &user.user_id,
            ReauthenticationPurpose::DeviceTrust,
            &trust.proof,
        )
        .await
        .expect_err("proof replay must fail");
    assert_eq!(replay.0, StatusCode::FORBIDDEN);

    let reenroll = service
        .reauthenticate(
            &user.user_id,
            ReauthenticationRequestData {
                password: password.to_string(),
                purpose: ReauthenticationPurpose::DeviceReenroll,
            },
        )
        .await
        .expect("second purpose gets an independent proof");
    let cross_owner = service
        .consume_reauthentication_proof(
            "another-owner",
            ReauthenticationPurpose::DeviceReenroll,
            &reenroll.proof,
        )
        .await
        .expect_err("proof cannot cross owners");
    assert_eq!(cross_owner.0, StatusCode::FORBIDDEN);
    service
        .consume_reauthentication_proof(
            &user.user_id,
            ReauthenticationPurpose::DeviceReenroll,
            &reenroll.proof,
        )
        .await
        .expect("cross-owner rejection must not consume the owner's proof");

    let expired = service
        .reauthenticate(
            &user.user_id,
            ReauthenticationRequestData {
                password: password.to_string(),
                purpose: ReauthenticationPurpose::DeviceTrust,
            },
        )
        .await
        .expect("expiry fixture proof");
    sqlx::query(
        "UPDATE auth_reauthentication_proofs
         SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE user_id = ? AND proof_hash = ?",
    )
    .bind(&user.user_id)
    .bind(format!("{:x}", Sha256::digest(expired.proof.as_bytes())))
    .execute(shared_pool.get())
    .await
    .expect("expire proof without wall-clock sleeping");
    let expired_error = service
        .consume_reauthentication_proof(
            &user.user_id,
            ReauthenticationPurpose::DeviceTrust,
            &expired.proof,
        )
        .await
        .expect_err("expired proof must fail closed");
    assert_eq!(expired_error.0, StatusCode::FORBIDDEN);

    sqlx::query("DELETE FROM auth_reauthentication_proofs WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(shared_pool.get())
        .await
        .expect("cleanup proofs");
    sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(shared_pool.get())
        .await
        .expect("cleanup roles");
    sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
        .bind(&user.user_id)
        .execute(shared_pool.get())
        .await
        .expect("cleanup user");
}
