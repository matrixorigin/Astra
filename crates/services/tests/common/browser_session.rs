//! Shared signed/encrypted fixture for real database and real Next rendering tests.
use astra_core::SharedPool;
use astra_services::FernetTokenEncryptor;
use sha2::{Digest, Sha256};
pub const JWT_KEY: &str = "browser-session-isolated-test-jwt";
pub const ENCRYPTION_KEY: &str = "browser-session-isolated-test-encryption";
pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// Expire only the access credential using the fixture issuer, preserving the
/// real owner/refresh transaction. No clock sleeps or production test hooks.
pub async fn expire_access(pool: &SharedPool, value: &str) -> String {
    age_access(pool, value, 360, -60, None).await
}

pub async fn age_access(
    pool: &SharedPool,
    value: &str,
    age: i64,
    remaining: i64,
    origin: Option<&str>,
) -> String {
    let encrypted: String = sqlx::query_scalar(
        "SELECT browser_ciphertext FROM auth_refresh_tokens WHERE browser_handle_hash = ?",
    )
    .bind(digest(value))
    .fetch_one(pool.get())
    .await
    .unwrap();
    let enc = FernetTokenEncryptor::new(ENCRYPTION_KEY).unwrap();
    let mut payload: serde_json::Value =
        serde_json::from_str(&enc.decrypt(&encrypted).unwrap()).unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = false;
    let mut claims = jsonwebtoken::decode::<serde_json::Value>(
        payload["access_token"].as_str().unwrap(),
        &jsonwebtoken::DecodingKey::from_secret(JWT_KEY.as_bytes()),
        &validation,
    )
    .unwrap()
    .claims;
    claims["exp"] = (chrono::Utc::now().timestamp() + remaining).into();
    claims["iat"] = (chrono::Utc::now().timestamp() - age).into();
    if let Some(origin) = origin {
        claims["origin"] = origin.into();
        let mut refresh = jsonwebtoken::decode::<serde_json::Value>(
            payload["refresh_token"].as_str().unwrap(),
            &jsonwebtoken::DecodingKey::from_secret(JWT_KEY.as_bytes()),
            &validation,
        )
        .unwrap()
        .claims;
        refresh["origin"] = origin.into();
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &refresh,
            &jsonwebtoken::EncodingKey::from_secret(JWT_KEY.as_bytes()),
        )
        .unwrap();
        sqlx::query("UPDATE auth_refresh_tokens SET token_hash = ? WHERE session_id = ?")
            .bind(digest(&token))
            .bind(payload["session_id"].as_str().unwrap())
            .execute(pool.get())
            .await
            .unwrap();
        payload["refresh_token"] = token.into();
    }
    payload["access_token"] = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_KEY.as_bytes()),
    )
    .unwrap()
    .into();
    sqlx::query(
        "UPDATE auth_refresh_tokens SET browser_ciphertext = ? WHERE browser_handle_hash = ?",
    )
    .bind(enc.encrypt(&payload.to_string()).unwrap())
    .bind(digest(value))
    .execute(pool.get())
    .await
    .unwrap();
    payload["refresh_token"].as_str().unwrap().to_owned()
}
