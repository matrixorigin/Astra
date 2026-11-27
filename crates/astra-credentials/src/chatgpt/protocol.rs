//! Shared OAuth protocol only. Database/local storage, platform identity,
//! refresh authority and provider I/O stay with their respective owners.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{API_BASE, ISSUER, SCOPES};

pub const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
pub const AUTHORIZATION_ENDPOINT: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const DISCOVERY_ENDPOINT: &str = "https://auth.openai.com/.well-known/openid-configuration";

pub struct AuthorizationRequest {
    pub client_id: String,
    pub host_id: String,
    pub redirect_uri: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub consent: bool,
}

impl std::fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .finish_non_exhaustive()
    }
}

fn random_value() -> String {
    // Three UUIDv4s provide more than 256 random bits within PKCE's 128-byte
    // verifier limit, without relying on a process-global random seed.
    format!(
        "{}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

impl AuthorizationRequest {
    pub fn new(
        host_id: String,
        client_id: Option<String>,
        redirect_uri: String,
    ) -> Result<Self, &'static str> {
        validate_redirect(&redirect_uri)?;
        let host = host_id
            .strip_prefix("urn:uuid:")
            .ok_or("Invalid ChatGPT host identity")?;
        Uuid::parse_str(host).map_err(|_| "Invalid ChatGPT host identity")?;
        let client_id = client_id.unwrap_or_else(|| DYNAMIC_CLIENT.into());
        validate_client_id(&client_id)?;
        Ok(Self {
            host_id,
            client_id,
            redirect_uri,
            state: random_value(),
            nonce: random_value(),
            verifier: random_value(),
            consent: false,
        })
    }

    /// The caller owns endpoint trust. Production device code always supplies
    /// AUTHORIZATION_ENDPOINT; Server uses its already-validated configuration.
    pub fn authorization_url(&self, endpoint: reqwest::Url) -> reqwest::Url {
        let mut url = endpoint;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("client_id", &self.client_id)
            .append_pair("ext_agent_host_id", &self.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", API_BASE)
            .append_pair("state", &self.state)
            .append_pair("nonce", &self.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
        if self.client_id == DYNAMIC_CLIENT {
            pairs.append_pair("agent_name_hint", "Astra");
        }
        if self.consent {
            pairs.append_pair("prompt", "consent");
        }
        drop(pairs);
        url
    }

    pub fn callback(&self, query: &str) -> Result<AuthorizationCode, &'static str> {
        if query.len() > 32 * 1024 {
            return Err("ChatGPT callback exceeds its limit");
        }
        let mut values = std::collections::BTreeMap::new();
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if values
                .insert(name.into_owned(), value.into_owned())
                .is_some()
            {
                return Err("ChatGPT callback contains duplicate fields");
            }
        }
        if values.get("state") != Some(&self.state) {
            return Err("ChatGPT callback state does not match");
        }
        if values.contains_key("code") == values.contains_key("error") {
            return Err("ChatGPT callback must contain either a code or an error");
        }
        if values.contains_key("error") {
            return Err("ChatGPT authorization was declined");
        }
        let code = values
            .remove("code")
            .ok_or("ChatGPT callback code is missing")?;
        if code.is_empty() || code.len() > 16_384 || code.chars().any(char::is_control) {
            return Err("ChatGPT callback code is invalid");
        }
        let returned_client = values.remove("client_id");
        let client_id = if self.client_id == DYNAMIC_CLIENT {
            returned_client.ok_or("ChatGPT callback did not issue a client registration")?
        } else {
            if returned_client
                .as_ref()
                .is_some_and(|id| id != &self.client_id)
            {
                return Err("ChatGPT callback changed the selected registration");
            }
            self.client_id.clone()
        };
        validate_client_id(&client_id)?;
        if client_id == DYNAMIC_CLIENT {
            return Err("ChatGPT callback did not issue a client registration");
        }
        Ok(AuthorizationCode { code, client_id })
    }
}

pub struct AuthorizationCode {
    pub code: String,
    pub client_id: String,
}

impl std::fmt::Debug for AuthorizationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationCode").finish_non_exhaustive()
    }
}

fn validate_client_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty()
        || value.len() > 255
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err("ChatGPT client registration is invalid");
    }
    Ok(())
}

pub fn validate_redirect(value: &str) -> Result<(), &'static str> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid OAuth redirect URI")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/auth/callback"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("OAuth redirect must be http://127.0.0.1:<port>/auth/callback");
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Identity {
    pub sub: String,
    pub nonce: Option<String>,
    pub email: Option<String>,
}

pub fn identity_key_id(id_token: &str) -> Result<String, &'static str> {
    if id_token.len() > 65_536 {
        return Err("ChatGPT identity token exceeds its limit");
    }
    let header = decode_header(id_token).map_err(|_| "Invalid ChatGPT identity token")?;
    if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
        return Err("Unsupported ChatGPT identity signature");
    }
    header
        .kid
        .filter(|id| !id.is_empty() && id.len() <= 255)
        .ok_or("ChatGPT identity signing key is missing")
}

/// Verify the signed identity before a caller checks its own nonce binding.
/// The issuer parameter belongs to validated configuration, never a JWT claim.
pub fn verify_identity(
    id_token: &str,
    client_id: &str,
    issuer: &str,
    keys: &JwkSet,
) -> Result<Identity, &'static str> {
    let kid = identity_key_id(id_token)?;
    let header = decode_header(id_token).map_err(|_| "Invalid ChatGPT identity token")?;
    let key = keys
        .find(&kid)
        .ok_or("ChatGPT identity signing key is unavailable")?;
    let key = jsonwebtoken::DecodingKey::from_jwk(key)
        .map_err(|_| "Invalid ChatGPT identity signing key")?;
    let mut validation = Validation::new(header.alg);
    validation.leeway = 0;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
    let identity = decode::<Identity>(id_token, &key, &validation)
        .map_err(|_| "ChatGPT identity verification failed")?
        .claims;
    if identity.sub.is_empty()
        || identity.sub.len() > 255
        || identity.email.as_ref().is_some_and(|v| v.len() > 255)
    {
        return Err("ChatGPT identity claims are invalid");
    }
    Ok(identity)
}

pub fn verify_device_identity(
    id_token: &str,
    client_id: &str,
    nonce: &str,
    keys: &JwkSet,
) -> Result<Identity, &'static str> {
    let identity = verify_identity(id_token, client_id, ISSUER, keys)?;
    if identity.nonce.as_deref() != Some(nonce) {
        return Err("ChatGPT identity nonce does not match");
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(client: Option<&str>) -> AuthorizationRequest {
        AuthorizationRequest::new(
            Uuid::new_v4().urn().to_string(),
            client.map(str::to_owned),
            "http://127.0.0.1:44551/auth/callback".into(),
        )
        .unwrap()
    }

    #[test]
    fn authorization_uses_pkce_without_exposing_verifier() {
        let request = request(None);
        let url = request.authorization_url(reqwest::Url::parse(AUTHORIZATION_ENDPOINT).unwrap());
        let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["client_id"], DYNAMIC_CLIENT);
        assert_eq!(pairs["resource"], API_BASE);
        assert_eq!(
            pairs["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(request.verifier.as_bytes()))
        );
        assert!(!url.as_str().contains(&request.verifier));
        assert!(!format!("{request:?}").contains(&request.verifier));
        assert!((43..=128).contains(&request.verifier.len()));
    }

    #[test]
    fn callback_rejects_duplicate_fields_and_registration_substitution() {
        let request = request(Some("issued-client"));
        for fields in [
            "code=synthetic-code&state=wrong",
            "code=x&error=denied",
            "code=x&code=y",
            "code=x&client_id=other",
            "code=x&state=duplicate",
        ] {
            assert!(
                request
                    .callback(&format!("state={}&{fields}", request.state))
                    .is_err()
            );
        }
        let code = request
            .callback(&format!("state={}&code=synthetic-code", request.state))
            .unwrap();
        assert_eq!(code.client_id, "issued-client");
        assert!(!format!("{code:?}").contains("synthetic-code"));
    }

    #[test]
    fn first_registration_requires_a_real_issued_client() {
        let request = request(None);
        assert!(
            request
                .callback(&format!("state={}&code=x", request.state))
                .is_err()
        );
        assert!(
            request
                .callback(&format!(
                    "state={}&code=x&client_id=dynamic_agent_client",
                    request.state
                ))
                .is_err()
        );
        assert_eq!(
            request
                .callback(&format!(
                    "state={}&code=x&client_id=issued-client",
                    request.state
                ))
                .unwrap()
                .client_id,
            "issued-client"
        );
    }

    #[test]
    fn redirects_are_loopback_only_and_fixed_path() {
        for bad in [
            "https://remote.invalid/auth/callback",
            "http://localhost:1234/auth/callback",
            "http://127.0.0.1:1234/other",
            "http://127.0.0.1:1234/auth/callback?token=secret",
        ] {
            assert!(validate_redirect(bad).is_err());
        }
    }
}
