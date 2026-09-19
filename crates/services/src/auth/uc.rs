//! UC native sessions are verified user identities, never MOI runtime grants.
use super::{
    AuthHttpError, AuthPrincipal, AuthPrincipalOrigin, AuthUserRecord, DatabaseAuthService,
};
use astra_core::{config::UcNativeSettings, error_response_coded, internal_error};
use axum::http::{HeaderMap, StatusCode};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

const SERVICE_CLIENT: &str = "astra-api-rs";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UcDiscovery {
    pub issuer: String,
    pub client_id: String,
    pub moi_api_url: String,
    pub scope: String,
    pub resource: String,
}

#[derive(Clone)]
pub struct UcNativeProvider {
    pub(crate) settings: UcNativeSettings,
    pub(crate) client: reqwest::Client,
    service_token: Arc<Mutex<Option<(String, std::time::Instant)>>>,
}

#[derive(Deserialize)]
pub struct UcIdentity {
    pub issuer: String,
    pub subject: String,
    pub session_id: String,
    pub client_id: String,
    pub audience: String,
    pub expires_at: i64,
    pub email: String,
    pub display_name: String,
}

fn unavailable() -> AuthHttpError {
    error_response_coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "UC authentication service unavailable",
        "uc_unavailable",
    )
}

fn invalid() -> AuthHttpError {
    error_response_coded(
        StatusCode::UNAUTHORIZED,
        "UC session is invalid or revoked; log in again",
        "uc_session_invalid",
    )
}

/// Only pinned HTTPS endpoints, or explicit local-development loopback HTTP.
pub fn validate_uc_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid UC integration URL".to_string())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "UC integration requires HTTPS (HTTP is only allowed for loopback development)".into(),
        );
    }
    Ok(url)
}

impl UcNativeProvider {
    /// Stable per-issuer memory namespace; Memoria owners are limited to 64
    /// bytes, while Astra's canonical external user IDs are 68 bytes.
    pub(crate) fn memory_owner(&self, subject: &str) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(format!("{}\0{subject}", self.settings.issuer));
        format!("uc_{}", URL_SAFE_NO_PAD.encode(digest))
    }

    /// Memory authority is account-scoped, not a retained CLI session token.
    /// Recheck UC on each operation, including background writes after login.
    pub(crate) async fn memory_account_active(&self, subject: &str) -> Result<bool, String> {
        let mut url = validate_uc_url(&self.settings.adapter_url)?;
        url.path_segments_mut()
            .map_err(|_| "Invalid UC adapter URL")?
            .extend(["api", "v1", "uc", "internal", "users", subject, "status"]);
        let bearer = self
            .service_bearer()
            .await
            .map_err(|_| "UC memory authority unavailable")?;
        let response = self
            .client
            .get(url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|_| "UC memory authority unavailable")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        #[derive(Deserialize)]
        struct Envelope {
            code: String,
            data: Account,
        }
        #[derive(Deserialize)]
        struct Account {
            uc_sub: String,
            status: String,
        }
        let result: Envelope = Self::json(response)
            .await
            .map_err(|_| "UC memory authority unavailable")?;
        if result.code != "OK" || result.data.uc_sub != subject {
            return Err("UC memory authority identity mismatch".into());
        }
        match result.data.status.as_str() {
            "active" => Ok(true),
            "pending_verification" | "disabled" | "deleted" => Ok(false),
            _ => Err("UC memory authority status invalid".into()),
        }
    }

    pub fn new(settings: UcNativeSettings) -> Result<Self, String> {
        for value in [
            &settings.issuer,
            &settings.adapter_url,
            &settings.moi_api_url,
            &settings.genesis_url,
        ] {
            validate_uc_url(value)?;
            if value.ends_with('/') {
                return Err("UC integration URLs must not end with a slash".into());
            }
        }
        if settings.client_secret.is_empty() {
            return Err("UC service credentials are required".into());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot create UC client")?;
        Ok(Self {
            settings,
            client,
            service_token: Arc::new(Mutex::new(None)),
        })
    }

    pub fn discovery(&self) -> UcDiscovery {
        UcDiscovery {
            issuer: self.settings.issuer.clone(),
            client_id: "astra-cli".into(),
            moi_api_url: self.settings.moi_api_url.clone(),
            scope: "openid profile email astra:user aistudio:user".into(),
            resource: "astra-api".into(),
        }
    }

    /// Untrusted issuer is used only for routing. Acceptance always requires
    /// the online UC verifier; failure never falls back to another provider.
    pub fn recognizes(&self, token: &str) -> bool {
        if token.len() > 16 * 1024 {
            return false;
        }
        token
            .split('.')
            .nth(1)
            .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| {
                v.get("iss").and_then(|i| i.as_str()) == Some(self.settings.issuer.as_str())
            })
    }

    pub(crate) async fn service_bearer(&self) -> Result<String, AuthHttpError> {
        let mut cached = self.service_token.lock().await;
        if let Some((value, until)) = &*cached
            && *until > std::time::Instant::now()
        {
            return Ok(value.clone());
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
            token_type: String,
        }
        let response = self
            .client
            .post(format!(
                "{}/protocol/openid-connect/token",
                self.settings.issuer
            ))
            .basic_auth(SERVICE_CLIENT, Some(&self.settings.client_secret))
            .form(&[
                ("grant_type", "client_credentials"),
                ("resource", "uc-management-api"),
                ("scope", "uc:native-sessions:resolve uc:users:status:read"),
            ])
            .send()
            .await
            .map_err(|_| unavailable())?;
        let token: Token = Self::json(response).await?;
        if token.access_token.is_empty()
            || !token.token_type.eq_ignore_ascii_case("bearer")
            || token.expires_in <= 30
            || token.expires_in > 86400
        {
            return Err(unavailable());
        }
        *cached = Some((
            token.access_token.clone(),
            std::time::Instant::now() + Duration::from_secs(token.expires_in - 30),
        ));
        Ok(token.access_token)
    }

    pub(crate) async fn json<T: DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, AuthHttpError> {
        Self::json_bounded(response, 64 * 1024).await
    }

    pub(crate) async fn json_bounded<T: DeserializeOwned>(
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<T, AuthHttpError> {
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(invalid());
        }
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if body.len() + chunk.len() > limit {
                return Err(unavailable());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| unavailable())
    }

    pub async fn verify(&self, token: &str) -> Result<UcIdentity, AuthHttpError> {
        #[derive(Deserialize)]
        struct Envelope {
            code: String,
            data: UcIdentity,
        }
        let response = self
            .client
            .post(format!(
                "{}/api/v1/uc/internal/native-access-tokens/resolve",
                self.settings.adapter_url
            ))
            .bearer_auth(self.service_bearer().await?)
            .json(&serde_json::json!({"access_token": token}))
            .send()
            .await
            .map_err(|_| unavailable())?;
        let result: Envelope = Self::json(response).await?;
        let identity = result.data;
        if result.code != "OK"
            || identity.issuer != self.settings.issuer
            || identity.client_id != "astra-cli"
            || identity.audience != "astra-api"
            || identity.subject.is_empty()
            || identity.subject.len() > 128
            || identity.session_id.is_empty()
            || identity.expires_at <= chrono::Utc::now().timestamp()
        {
            return Err(invalid());
        }
        Ok(identity)
    }
}

impl DatabaseAuthService {
    pub fn with_uc_native(mut self, provider: Option<UcNativeProvider>) -> Self {
        self.uc_provider = provider;
        self
    }

    pub(super) async fn principal_from_uc(
        &self,
        headers: &HeaderMap,
        token: &str,
        provider: &UcNativeProvider,
    ) -> Result<AuthPrincipal, AuthHttpError> {
        if headers.get_all("authorization").iter().count() != 1
            || headers.contains_key("cookie")
            || headers.contains_key("x-api-key")
        {
            return Err(invalid());
        }
        let identity = provider.verify(token).await?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.ensure_default_roles(&pool)
            .await
            .map_err(internal_error)?;
        let mut tx = pool.begin().await.map_err(internal_error)?;
        let provider_id = format!("uc:{}", provider.settings.issuer);
        let user = self
            .resolve_verified_provider_identity(&mut tx, &provider_id, &identity.subject, None)
            .await?;
        tx.commit().await.map_err(internal_error)?;
        Ok(AuthPrincipal {
            user: AuthUserRecord {
                user_id: user.user_id,
                username: user.username,
                email: identity.email,
                display_name: Some(identity.display_name),
            },
            session_id: Some(identity.session_id),
            origin: AuthPrincipalOrigin::VerifiedProvider {
                provider_id,
                external_subject: identity.subject,
            },
        })
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use astra_core::{JwtSettings, MatrixOneSettings, MemoriaSettings};

    fn provider(issuer: &str) -> UcNativeProvider {
        UcNativeProvider::new(UcNativeSettings {
            issuer: issuer.into(),
            adapter_url: "https://uc.example.test".into(),
            client_secret: "test-service-secret".into(),
            moi_api_url: "https://moi.example.test/newmoi".into(),
            genesis_url: "https://genesis.example.test".into(),
            builtin_memory: true,
        })
        .unwrap()
    }

    #[test]
    fn memory_namespace_is_stable_bounded_and_issuer_scoped() {
        let first = provider("https://uc.example.test/realms/one");
        let second = provider("https://uc.example.test/realms/two");
        let owner = first.memory_owner("alice");
        assert_eq!(owner.len(), 46);
        assert!(owner.starts_with("uc_"));
        assert!(
            owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        );
        assert_eq!(owner, first.memory_owner("alice"));
        assert_ne!(owner, first.memory_owner("bob"));
        assert_ne!(owner, second.memory_owner("alice"));
        assert_eq!(first.memory_owner(&"a".repeat(128)).len(), 46);
    }

    #[test]
    fn builtin_memory_requires_explicit_server_credentials_without_external_login() {
        let memory = MemoriaSettings {
            base_url: "http://memoria:8100".into(),
            master_key: Some("test-memory-secret".into()),
            self_hosted_master_access: false,
            issuer: None,
            web_url: None,
            legacy_issuer: None,
        };
        let auth = || {
            DatabaseAuthService::new(
                MatrixOneSettings::default(),
                JwtSettings {
                    secret_key: "test-jwt".into(),
                    algorithm: "HS256".into(),
                    access_token_expire_minutes: 15,
                    refresh_token_expire_days: 7,
                },
            )
            .with_uc_native(Some(provider("https://uc.example.test/realms/one")))
        };
        assert!(auth().with_memoria_settings(&memory).is_ok());
        for invalid in [
            MemoriaSettings {
                master_key: None,
                ..memory.clone()
            },
            MemoriaSettings {
                master_key: Some(String::new()),
                ..memory.clone()
            },
            MemoriaSettings {
                base_url: String::new(),
                ..memory.clone()
            },
            MemoriaSettings {
                web_url: Some("https://memory.example.test".into()),
                ..memory.clone()
            },
        ] {
            assert!(auth().with_memoria_settings(&invalid).is_err());
        }
        let debug = format!(
            "{:?}",
            super::super::memoria::MemoriaProvider::new(&memory).unwrap()
        );
        assert!(!debug.contains("test-memory-secret"));
    }
}
