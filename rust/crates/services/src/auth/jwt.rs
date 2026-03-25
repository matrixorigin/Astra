use axum::{Json, http::StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use mo_agent_core::{ErrorResponse, JwtSettings, error_response, internal_error};
use serde::{Deserialize, Serialize};

pub(super) fn decode_jwt_claims_with_detail(
    token: &str,
    settings: &JwtSettings,
    invalid_detail: &'static str,
) -> Result<JwtClaims, (StatusCode, Json<ErrorResponse>)> {
    let validation = Validation::new(parse_jwt_algorithm(&settings.algorithm)?);
    decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(settings.secret_key.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(|_| error_response(StatusCode::UNAUTHORIZED, invalid_detail))
}

pub(super) fn decode_jwt_claims(
    token: &str,
    settings: &JwtSettings,
) -> Result<JwtClaims, (StatusCode, Json<ErrorResponse>)> {
    decode_jwt_claims_with_detail(token, settings, "Could not validate credentials")
}

fn parse_jwt_algorithm(algorithm: &str) -> Result<Algorithm, (StatusCode, Json<ErrorResponse>)> {
    match algorithm {
        "HS256" => Ok(Algorithm::HS256),
        "HS384" => Ok(Algorithm::HS384),
        "HS512" => Ok(Algorithm::HS512),
        _ => Err(internal_error(format!(
            "unsupported JWT algorithm configured: {algorithm}"
        ))),
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct JwtClaims {
    pub(super) sub: Option<String>,
    pub(super) username: Option<String>,
    #[serde(rename = "type")]
    pub(super) token_type: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct JwtTokenClaims {
    pub(super) sub: String,
    pub(super) username: Option<String>,
    #[serde(rename = "type")]
    pub(super) token_type: String,
    pub(super) exp: usize,
    pub(super) iat: usize,
}

pub(super) fn create_jwt_token(
    settings: &JwtSettings,
    mut claims: JwtTokenClaims,
    expires_in: ChronoDuration,
) -> Result<String, String> {
    let now = Utc::now();
    claims.iat = now.timestamp() as usize;
    claims.exp = (now + expires_in).timestamp() as usize;
    let algorithm =
        parse_jwt_algorithm(&settings.algorithm).map_err(|error| error.1.detail.clone())?;
    encode(
        &Header::new(algorithm),
        &claims,
        &EncodingKey::from_secret(settings.secret_key.as_bytes()),
    )
    .map_err(|error| error.to_string())
}
