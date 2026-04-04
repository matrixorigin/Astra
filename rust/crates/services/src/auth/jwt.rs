use astra_core::{ErrorResponse, JwtSettings, error_response, internal_error};
use axum::{Json, http::StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
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
    /// Random JWT ID — ensures tokens issued in the same second are unique.
    pub(super) jti: String,
}

pub(super) fn create_jwt_token(
    settings: &JwtSettings,
    mut claims: JwtTokenClaims,
    expires_in: ChronoDuration,
) -> Result<String, String> {
    let now = Utc::now();
    claims.iat = now.timestamp() as usize;
    claims.exp = (now + expires_in).timestamp() as usize;
    claims.jti = uuid::Uuid::new_v4().to_string();
    let algorithm =
        parse_jwt_algorithm(&settings.algorithm).map_err(|error| error.1.detail.clone())?;
    encode(
        &Header::new(algorithm),
        &claims,
        &EncodingKey::from_secret(settings.secret_key.as_bytes()),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unwrap a JWT result whose error type lacks Debug.
    fn unwrap_jwt<T>(result: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
        match result {
            Ok(v) => v,
            Err((status, body)) => {
                panic!("JWT operation failed: {} - {}", status, body.detail)
            }
        }
    }

    fn test_settings(algorithm: &str) -> JwtSettings {
        JwtSettings {
            secret_key: "test-secret-key-for-unit-tests".into(),
            algorithm: algorithm.into(),
            access_token_expire_minutes: 60,
            refresh_token_expire_days: 7,
        }
    }

    fn make_claims(sub: &str, token_type: &str) -> JwtTokenClaims {
        JwtTokenClaims {
            sub: sub.into(),
            username: Some("testuser".into()),
            token_type: token_type.into(),
            exp: 0,
            iat: 0,
            jti: String::new(),
        }
    }

    #[test]
    fn encode_decode_roundtrip_hs256() {
        let settings = test_settings("HS256");
        let claims = make_claims("user-123", "access");
        let token = create_jwt_token(&settings, claims, ChronoDuration::minutes(30)).unwrap();
        let decoded = unwrap_jwt(decode_jwt_claims(&token, &settings));
        assert_eq!(decoded.sub.as_deref(), Some("user-123"));
        assert_eq!(decoded.username.as_deref(), Some("testuser"));
        assert_eq!(decoded.token_type.as_deref(), Some("access"));
    }

    #[test]
    fn encode_decode_roundtrip_hs384() {
        let settings = test_settings("HS384");
        let claims = make_claims("user-384", "refresh");
        let token = create_jwt_token(&settings, claims, ChronoDuration::hours(1)).unwrap();
        let decoded = unwrap_jwt(decode_jwt_claims(&token, &settings));
        assert_eq!(decoded.sub.as_deref(), Some("user-384"));
        assert_eq!(decoded.token_type.as_deref(), Some("refresh"));
    }

    #[test]
    fn encode_decode_roundtrip_hs512() {
        let settings = test_settings("HS512");
        let claims = make_claims("user-512", "access");
        let token = create_jwt_token(&settings, claims, ChronoDuration::hours(2)).unwrap();
        let decoded = unwrap_jwt(decode_jwt_claims(&token, &settings));
        assert_eq!(decoded.sub.as_deref(), Some("user-512"));
    }

    #[test]
    fn decode_gibberish_token_fails() {
        let settings = test_settings("HS256");
        let result = decode_jwt_claims("not.a.valid.jwt.token", &settings);
        assert!(result.is_err());
        let (status, body) = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.detail, "Could not validate credentials");
    }

    #[test]
    fn decode_with_wrong_secret_fails() {
        let settings = test_settings("HS256");
        let claims = make_claims("user-1", "access");
        let token = create_jwt_token(&settings, claims, ChronoDuration::minutes(30)).unwrap();

        let bad_settings = JwtSettings {
            secret_key: "wrong-secret".into(),
            ..settings
        };
        let result = decode_jwt_claims(&token, &bad_settings);
        assert!(result.is_err());
    }

    #[test]
    fn decode_with_detail_returns_custom_message() {
        let settings = test_settings("HS256");
        let result = decode_jwt_claims_with_detail("garbage", &settings, "Custom invalid message");
        let (status, body) = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.detail, "Custom invalid message");
    }

    #[test]
    fn parse_jwt_algorithm_known_variants() {
        assert_eq!(unwrap_jwt(parse_jwt_algorithm("HS256")), Algorithm::HS256);
        assert_eq!(unwrap_jwt(parse_jwt_algorithm("HS384")), Algorithm::HS384);
        assert_eq!(unwrap_jwt(parse_jwt_algorithm("HS512")), Algorithm::HS512);
    }

    #[test]
    fn parse_jwt_algorithm_unsupported() {
        let result = parse_jwt_algorithm("RS256");
        let (status, body) = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.detail.contains("unsupported JWT algorithm"));
    }

    #[test]
    fn parse_jwt_algorithm_empty_string() {
        let result = parse_jwt_algorithm("");
        assert!(result.is_err());
    }

    #[test]
    fn create_token_with_unsupported_algorithm_fails() {
        let settings = test_settings("RS256");
        let claims = make_claims("user-1", "access");
        let result = create_jwt_token(&settings, claims, ChronoDuration::minutes(30));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported JWT algorithm"));
    }

    #[test]
    fn claims_without_optional_fields() {
        let settings = test_settings("HS256");
        let claims = JwtTokenClaims {
            sub: "user-minimal".into(),
            username: None,
            token_type: "access".into(),
            exp: 0,
            iat: 0,
            jti: String::new(),
        };
        let token = create_jwt_token(&settings, claims, ChronoDuration::minutes(30)).unwrap();
        let decoded = unwrap_jwt(decode_jwt_claims(&token, &settings));
        assert_eq!(decoded.sub.as_deref(), Some("user-minimal"));
        assert!(decoded.username.is_none());
    }

    #[test]
    fn decode_empty_token_fails() {
        let settings = test_settings("HS256");
        let result = decode_jwt_claims("", &settings);
        assert!(result.is_err());
    }

    #[test]
    fn token_timestamps_are_set() {
        let settings = test_settings("HS256");
        let claims = make_claims("user-ts", "access");
        let token = create_jwt_token(&settings, claims, ChronoDuration::minutes(5)).unwrap();

        // Decode raw to verify exp/iat are set
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = false;
        let data = decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret(settings.secret_key.as_bytes()),
            &validation,
        )
        .unwrap();
        let iat = data.claims["iat"].as_u64().unwrap();
        let exp = data.claims["exp"].as_u64().unwrap();
        assert!(exp > iat);
        // 5 minutes = 300 seconds
        assert_eq!(exp - iat, 300);
    }

    #[test]
    fn two_tokens_for_same_user_are_unique() {
        // Regression: same user logging in twice in the same second must produce
        // different tokens (and thus different token_hash values in the DB).
        let settings = test_settings("HS256");
        let t1 = create_jwt_token(
            &settings,
            make_claims("u1", "refresh"),
            ChronoDuration::days(7),
        )
        .unwrap();
        let t2 = create_jwt_token(
            &settings,
            make_claims("u1", "refresh"),
            ChronoDuration::days(7),
        )
        .unwrap();
        assert_ne!(t1, t2, "tokens must differ due to unique jti");
    }
}
