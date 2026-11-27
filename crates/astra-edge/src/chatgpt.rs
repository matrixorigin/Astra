//! Device-owned OAuth I/O. No OpenAI credential or arbitrary endpoint is
//! accepted from a Server frame. Storage and rotation belong to credentials;
//! the protocol shared with Server remains pure.
use std::{sync::Arc, time::Duration};

use astra_credentials::chatgpt::{
    self, Connection, CredentialState, RefreshFailure, Registration, Store, Tokens,
    protocol::{self, AuthorizationRequest},
};
use futures_util::StreamExt;
use jsonwebtoken::jwk::JwkSet;
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

const BODY_LIMIT: usize = 1024 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("Device ChatGPT transport configuration is invalid")]
    TransportConfiguration,
    #[error("ChatGPT could not be reached from this device")]
    Unavailable,
    #[error("ChatGPT returned an invalid authorization response")]
    InvalidResponse,
    #[error("ChatGPT authorization was declined or the callback did not match")]
    InvalidCallback,
    #[error("ChatGPT identity could not be verified")]
    InvalidIdentity,
    #[error("ChatGPT rejected this client registration")]
    InvalidClient,
    #[error("ChatGPT plan permission is disabled; authorize plan usage from /model")]
    PlanDisabled,
    #[error(
        "OpenAI rejected this device connection (403). Check this ChatGPT account's access and the device network; MOI sign-in is unchanged."
    )]
    ProviderForbidden,
    #[error(
        "OpenAI did not accept the ChatGPT credential (401). Reauthorize the connection from /model; MOI sign-in is unchanged."
    )]
    AuthorizationRequired,
    #[error("OpenAI rate-limited this ChatGPT connection. Wait before trying again.")]
    RateLimited,
    #[error(
        "MOI sign-in changed; the ChatGPT grant was not published. Provider revocation confirmed: {revocation_confirmed}"
    )]
    PlatformChanged { revocation_confirmed: bool },
    #[error(
        "ChatGPT authorization was not published. Provider revocation confirmed: {revocation_confirmed}. Reopen /model to authorize again."
    )]
    GrantRejected { revocation_confirmed: bool },
    #[error(transparent)]
    Credentials(#[from] chatgpt::Error),
}

/// A validated grant is still local. Its caller must recheck platform session
/// authority before publishing it or changing the selected model.
pub struct AuthorizedGrant {
    registration: Registration,
    tokens: Tokens,
    plan_authorized: bool,
}

impl std::fmt::Debug for AuthorizedGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedGrant").finish_non_exhaustive()
    }
}

impl AuthorizedGrant {
    pub fn save(
        &self,
        store: &Store,
        id: Uuid,
        expected_revision: Option<u64>,
        label: String,
    ) -> Result<Connection, chatgpt::Error> {
        store.authorize(
            id,
            expected_revision,
            label,
            self.registration.clone(),
            self.tokens.clone(),
            self.plan_authorized,
        )
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    token_type: String,
    expires_in: i64,
    scope: Option<String>,
}

impl TokenResponse {
    fn tokens(&self) -> Result<Tokens, Error> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.expires_in <= 0
            || self.expires_in > 86_400 * 30
        {
            return Err(Error::InvalidResponse);
        }
        Tokens::new(
            self.access_token.clone(),
            self.refresh_token.clone(),
            now()?
                .checked_add(self.expires_in)
                .ok_or(Error::InvalidResponse)?,
        )
        .map_err(Error::from)
    }
}

fn now() -> Result<i64, Error> {
    astra_credentials::native::unix_now().map_err(|_| Error::Unavailable)
}

fn provider_status(status: reqwest::StatusCode) -> Error {
    match status {
        reqwest::StatusCode::UNAUTHORIZED => Error::AuthorizationRequired,
        reqwest::StatusCode::FORBIDDEN => Error::ProviderForbidden,
        reqwest::StatusCode::TOO_MANY_REQUESTS => Error::RateLimited,
        _ => Error::Unavailable,
    }
}

#[derive(Clone)]
struct Endpoints {
    api_base: String,
    issuer: String,
    token: String,
    discovery: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api_base: chatgpt::API_BASE.into(),
            issuer: chatgpt::ISSUER.into(),
            token: protocol::TOKEN_ENDPOINT.into(),
            discovery: protocol::DISCOVERY_ENDPOINT.into(),
        }
    }
}

#[derive(Clone)]
struct Metadata {
    fetched: std::time::Instant,
    keys: JwkSet,
    revocation_endpoint: reqwest::Url,
}

pub struct Client {
    http: reqwest::Client,
    endpoints: Endpoints,
    metadata: Mutex<Option<Metadata>>,
}

impl Client {
    pub(crate) fn responses_endpoint(&self) -> String {
        format!("{}/responses", self.endpoints.api_base)
    }

    #[cfg(test)]
    pub(crate) fn with_test_origin(origin: &str) -> Arc<Self> {
        Arc::new(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            endpoints: Endpoints {
                api_base: format!("{origin}/v1"),
                issuer: origin.into(),
                token: format!("{origin}/token"),
                discovery: format!("{origin}/.well-known/openid-configuration"),
            },
            metadata: Mutex::new(None),
        })
    }
    pub fn new() -> Result<Arc<Self>, Error> {
        let builder = astra_core::net::runner_provider_client_builder()
            .map_err(|_| Error::TransportConfiguration)?;
        let http = builder
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::TransportConfiguration)?;
        Ok(Arc::new(Self {
            http,
            endpoints: Endpoints::default(),
            metadata: Mutex::new(None),
        }))
    }

    pub fn begin(
        &self,
        host_id: String,
        registration: Option<&Registration>,
        redirect_uri: String,
    ) -> Result<AuthorizationRequest, Error> {
        if registration.is_some_and(|registration| registration.host_id != host_id) {
            return Err(Error::InvalidIdentity);
        }
        AuthorizationRequest::new(
            host_id,
            registration.map(|r| r.client_id.clone()),
            redirect_uri,
        )
        .map_err(|_| Error::InvalidCallback)
    }

    fn metadata_endpoint(&self, value: &str) -> Result<reqwest::Url, Error> {
        let url = reqwest::Url::parse(value).map_err(|_| Error::InvalidResponse)?;
        let issuer =
            reqwest::Url::parse(&self.endpoints.issuer).map_err(|_| Error::InvalidResponse)?;
        if url.origin() != issuer.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(url)
    }

    async fn metadata(&self) -> Result<Metadata, Error> {
        self.read_metadata(false).await
    }

    async fn read_metadata(&self, force: bool) -> Result<Metadata, Error> {
        let mut cached = self.metadata.lock().await;
        if let Some(value) = cached.as_ref()
            && !force
            && value.fetched.elapsed() < Duration::from_secs(300)
        {
            return Ok(value.clone());
        }
        let response = self
            .http
            .get(&self.endpoints.discovery)
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Unavailable);
        }
        let discovery: serde_json::Value =
            serde_json::from_slice(&body(response).await?).map_err(|_| Error::InvalidResponse)?;
        if discovery["issuer"].as_str() != Some(&self.endpoints.issuer) {
            return Err(Error::InvalidIdentity);
        }
        let jwks = self.metadata_endpoint(
            discovery["jwks_uri"]
                .as_str()
                .ok_or(Error::InvalidResponse)?,
        )?;
        let revocation_endpoint = self.metadata_endpoint(
            discovery["revocation_endpoint"]
                .as_str()
                .ok_or(Error::InvalidResponse)?,
        )?;
        let response = self
            .http
            .get(jwks)
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Unavailable);
        }
        let keys: JwkSet =
            serde_json::from_slice(&body(response).await?).map_err(|_| Error::InvalidResponse)?;
        if keys.keys.is_empty() || keys.keys.len() > 64 {
            return Err(Error::InvalidResponse);
        }
        let value = Metadata {
            fetched: std::time::Instant::now(),
            keys,
            revocation_endpoint,
        };
        *cached = Some(value.clone());
        Ok(value)
    }

    async fn identity(
        &self,
        token: &str,
        client_id: &str,
        metadata: &Metadata,
    ) -> Result<protocol::Identity, Error> {
        let kid = protocol::identity_key_id(token).map_err(|_| Error::InvalidIdentity)?;
        // Refresh signing metadata only; never repeat the consumed code or
        // rotating refresh token request when the provider changes keys.
        let replacement;
        let metadata = if metadata.keys.find(&kid).is_none() {
            replacement = self.read_metadata(true).await?;
            &replacement
        } else {
            metadata
        };
        protocol::verify_identity(token, client_id, &self.endpoints.issuer, &metadata.keys)
            .map_err(|_| Error::InvalidIdentity)
    }

    pub async fn exchange(
        &self,
        request: &AuthorizationRequest,
        callback: &str,
        expected: Option<&Registration>,
    ) -> Result<AuthorizedGrant, Error> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            if expected.is_some_and(|registration| {
                registration.client_id != request.client_id
                    || registration.host_id != request.host_id
            }) {
                return Err(Error::InvalidIdentity);
            }
            let code = request
                .callback(callback)
                .map_err(|_| Error::InvalidCallback)?;
            // Prepare identity verification before consuming a one-use code.
            let metadata = self.metadata().await?;
            let response = self
                .http
                .post(&self.endpoints.token)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", &code.client_id),
                    ("code", &code.code),
                    ("redirect_uri", &request.redirect_uri),
                    ("code_verifier", &request.verifier),
                    ("resource", chatgpt::API_BASE),
                ])
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(Error::InvalidResponse);
            }
            let token: TokenResponse = serde_json::from_slice(&body(response).await?)
                .map_err(|_| Error::InvalidResponse)?;
            let tokens = token.tokens()?;
            let identity = self
                .identity(
                    token.id_token.as_deref().unwrap_or_default(),
                    &code.client_id,
                    &metadata,
                )
                .await;
            let identity = match identity {
                Ok(identity)
                    if identity.nonce.as_deref() == Some(&request.nonce)
                        && expected
                            .is_none_or(|registration| registration.subject == identity.sub) =>
                {
                    identity
                }
                _ => {
                    let revoked = self.revoke_tokens(&code.client_id, &tokens).await;
                    return Err(Error::GrantRejected {
                        revocation_confirmed: revoked,
                    });
                }
            };
            let plan_authorized = token.scope.as_deref().is_some_and(|scopes| {
                scopes
                    .split_ascii_whitespace()
                    .any(|scope| scope == chatgpt::PLAN_SCOPE)
            });
            Ok(AuthorizedGrant {
                registration: Registration {
                    client_id: code.client_id,
                    subject: identity.sub,
                    host_id: request.host_id.clone(),
                },
                tokens,
                plan_authorized,
            })
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    /// Final local publication is serialized with platform logout. A browser
    /// completing after logout cannot recreate this user's device connection.
    pub async fn publish_native(
        &self,
        grant: AuthorizedGrant,
        reference: astra_credentials::native_device::NativeDeviceAttachment,
        scope: astra_credentials::LocalModelScope,
        id: Uuid,
        revision: Option<u64>,
        label: String,
    ) -> Result<Connection, Error> {
        let (result, abandoned) = tokio::task::spawn_blocking(move || {
            let authority = reference.resolve().and_then(|(store, session)| {
                if !reference.matches_scope(&scope) {
                    return Err("MOI device owner changed".into());
                }
                store.device_model_lease(&session)
            });
            let Ok(_lease) = authority else {
                return (
                    Err(Error::PlatformChanged {
                        revocation_confirmed: false,
                    }),
                    Some(grant),
                );
            };
            let result = grant.save(&Store::new(&scope), id, revision, label);
            // Only definitive pre-publication rejection permits cleanup. An
            // I/O error can be an already-committed rename; never revoke a
            // possibly committed connection based on ambiguous persistence.
            match result {
                // Another reauthorization won. Its provider registration may
                // share the token family; do not revoke the winner as cleanup.
                Err(chatgpt::Error::Changed) if revision.is_some() => (
                    Err(Error::GrantRejected {
                        revocation_confirmed: false,
                    }),
                    None,
                ),
                Err(
                    chatgpt::Error::Changed
                    | chatgpt::Error::Capacity
                    | chatgpt::Error::InvalidCredentials,
                ) => (
                    Err(Error::GrantRejected {
                        revocation_confirmed: false,
                    }),
                    Some(grant),
                ),
                other => (other.map_err(Error::from), None),
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?;
        if let Some(grant) = abandoned {
            let revoked = self
                .revoke_tokens(&grant.registration.client_id, &grant.tokens)
                .await;
            return Err(match result {
                Err(Error::PlatformChanged { .. }) => Error::PlatformChanged {
                    revocation_confirmed: revoked,
                },
                _ => Error::GrantRejected {
                    revocation_confirmed: revoked,
                },
            });
        }
        result
    }

    pub async fn catalog(
        self: &Arc<Self>,
        store: Store,
        id: Uuid,
    ) -> Result<Vec<astra_core::model_wire::responses::SubscriptionCatalogModel>, Error> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let (_, tokens) = self.credentials(store, id).await?;
            let response = self
                .http
                .get(format!("{}/models", self.endpoints.api_base))
                .bearer_auth(tokens.access_token())
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(provider_status(response.status()));
            }
            let value: serde_json::Value = serde_json::from_slice(&body(response).await?)
                .map_err(|_| Error::InvalidResponse)?;
            astra_core::model_wire::responses::subscription_catalog(&value)
                .map_err(|_| Error::InvalidResponse)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    pub async fn probe(
        self: &Arc<Self>,
        store: Store,
        id: Uuid,
        model: &str,
        output_limit: usize,
    ) -> Result<Vec<String>, Error> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let catalog = self.catalog(store.clone(), id).await?;
            let selected = catalog
                .into_iter()
                .find(|item| item.slug == model)
                .ok_or(Error::InvalidResponse)?;
            let (_, tokens) = self.credentials(store, id).await?;
            let request = astra_core::model_wire::responses::request(
                model,
                &[serde_json::json!({"role":"user","content":"Reply briefly with OK."})],
                None,
                &id.to_string(),
                true,
                Some(output_limit),
            )
            .map_err(|_| Error::InvalidResponse)?;
            let response = self
                .http
                .post(format!("{}/responses", self.endpoints.api_base))
                .bearer_auth(tokens.access_token())
                .json(&request)
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(provider_status(response.status()));
            }
            let bytes = body(response).await?;
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::InvalidResponse)?;
            let output = astra_core::model_wire::responses::decode_sse(text)
                .map_err(|_| Error::InvalidResponse)?;
            if output.finish_reason.as_deref() == Some("length") || output.text.trim().is_empty() {
                return Err(Error::InvalidResponse);
            }
            Ok(selected.supported_reasoning_efforts)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    /// One device credential snapshot. Cross-process contenders observe the
    /// durable intent, never resend its refresh token. The refresh task owns
    /// settlement even if its initiating terminal cancels this waiter.
    pub async fn credentials(
        self: &Arc<Self>,
        store: Store,
        id: Uuid,
    ) -> Result<(Connection, Tokens), Error> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            loop {
                let local = store.clone();
                let snapshot = tokio::task::spawn_blocking(move || local.token_snapshot(id))
                    .await.map_err(|_| Error::Unavailable)?;
                match snapshot {
                    Ok((connection, tokens)) if tokens.expires_at() > now()? + 60 => return Ok((connection, tokens)),
                    Ok((connection, _)) => {
                        self.refresh_or_observe(store.clone(), connection).await?;
                    }
                    Err(chatgpt::Error::RefreshPending) => {
                        let local = store.clone();
                        let connection = tokio::task::spawn_blocking(move || local.list())
                            .await.map_err(|_| Error::Unavailable)??
                            .into_iter().find(|c| c.connection_ref == id).ok_or(chatgpt::Error::NotFound)?;
                        if matches!(connection.credential_state, CredentialState::Refreshing { deadline, .. } if deadline <= now()?) {
                            self.refresh_or_observe(store.clone(), connection).await?;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }).await.map_err(|_| Error::Unavailable)?
    }

    async fn refresh_or_observe(
        self: &Arc<Self>,
        store: Store,
        connection: Connection,
    ) -> Result<(), Error> {
        // Discovery is bounded and prepared before the durable rotation intent.
        let metadata = self.metadata().await?;
        let local = store.clone();
        let refresh = tokio::task::spawn_blocking(move || {
            local.begin_refresh(
                connection.connection_ref,
                connection.authorization_revision,
                connection.token_version,
                astra_credentials::native::unix_now().map_err(|_| chatgpt::Error::Storage)?,
            )
        })
        .await
        .map_err(|_| Error::Unavailable)?;
        let refresh = match refresh {
            Ok(refresh) => refresh,
            Err(chatgpt::Error::RefreshPending | chatgpt::Error::Changed) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let client = self.clone();
        // The detached owner has an operation budget shorter than its 30s
        // durable lease and persists the exact outcome before releasing it.
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(20),
                client.refresh_exchange(&refresh, &metadata),
            )
            .await;
            match result {
                Ok(Ok(tokens)) => {
                    let replacement = tokens.clone();
                    let owner = store.clone();
                    let saved = tokio::task::spawn_blocking(move || {
                        owner.finish_refresh(&refresh, replacement)
                    })
                    .await
                    .map_err(|_| Error::Unavailable)?;
                    match saved {
                        Ok(_) => Ok(()),
                        Err(chatgpt::Error::Changed) => {
                            // A disconnect/reauthorization won. This old writer
                            // cannot revoke a registration that a new owner may
                            // now use. Drop the uncommitted grant locally; remote
                            // cleanup is explicitly unconfirmed, never success.
                            tracing::warn!(category = "authorization_changed", "Uncommitted ChatGPT refresh retired locally; provider cleanup unconfirmed");
                            Err(chatgpt::Error::Changed.into())
                        }
                        Err(error) => Err(error.into()),
                    }
                }
                failure => {
                    let (state, error) = match failure {
                        Ok(Err((failure, error))) => (failure, error),
                        Err(_) => (RefreshFailure::Uncertain, Error::Unavailable),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    tokio::task::spawn_blocking(move || store.fail_refresh(&refresh, state))
                        .await
                        .map_err(|_| Error::Unavailable)??;
                    Err(error)
                }
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    async fn refresh_exchange(
        &self,
        refresh: &chatgpt::Refresh,
        metadata: &Metadata,
    ) -> Result<Tokens, (RefreshFailure, Error)> {
        let uncertain = |error| (RefreshFailure::Uncertain, error);
        let request = self
            .http
            .post(&self.endpoints.token)
            .form(&[
                ("grant_type", "refresh_token"),
                (
                    "client_id",
                    refresh.connection.registration.client_id.as_str(),
                ),
                ("refresh_token", refresh.tokens.refresh_token()),
                ("resource", chatgpt::API_BASE),
            ])
            .build()
            .map_err(|_| (RefreshFailure::NotSent, Error::TransportConfiguration))?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| uncertain(Error::Unavailable))?;
        let status = response.status();
        let bytes = body(response).await.map_err(uncertain)?;
        if status != reqwest::StatusCode::OK {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| uncertain(Error::InvalidResponse))?;
            let code = value["error"]
                .as_str()
                .or_else(|| value["error"]["code"].as_str());
            return Err(match code {
                Some(
                    "invalid_grant"
                    | "invalid_refresh_token"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_invalidated"
                    | "refresh_token_reused",
                ) => (RefreshFailure::Rejected, chatgpt::Error::Reauthorize.into()),
                Some("invalid_client") => (RefreshFailure::NotSent, Error::InvalidClient),
                _ => uncertain(Error::Unavailable),
            });
        }
        let token: TokenResponse =
            serde_json::from_slice(&bytes).map_err(|_| uncertain(Error::InvalidResponse))?;
        if token.scope.as_deref().is_some_and(|scopes| {
            !scopes
                .split_ascii_whitespace()
                .any(|s| s == chatgpt::PLAN_SCOPE)
        }) {
            return Err((RefreshFailure::Rejected, Error::PlanDisabled));
        }
        if let Some(id_token) = &token.id_token {
            let identity = self
                .identity(
                    id_token,
                    &refresh.connection.registration.client_id,
                    metadata,
                )
                .await
                .map_err(uncertain)?;
            if identity.sub != refresh.connection.registration.subject {
                return Err(uncertain(Error::InvalidIdentity));
            }
        }
        token.tokens().map_err(uncertain)
    }

    async fn revoke_tokens(&self, client_id: &str, tokens: &Tokens) -> bool {
        let result = tokio::time::timeout(OPERATION_TIMEOUT, async {
            let metadata = self.metadata().await?;
            let response = self
                .http
                .post(metadata.revocation_endpoint)
                .form(&[
                    ("token", tokens.refresh_token()),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", client_id),
                ])
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            Ok::<_, Error>(response.status() == reqwest::StatusCode::OK)
        })
        .await;
        matches!(result, Ok(Ok(true)))
    }

    async fn disconnect_credentials(
        store: Store,
        id: Uuid,
    ) -> Result<(Connection, Option<Tokens>), Error> {
        tokio::task::spawn_blocking(move || store.disconnect(id))
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(Into::into)
    }

    /// Clear renewable credentials even when device proxy configuration is invalid.
    pub async fn disconnect_default(store: Store, id: Uuid) -> Result<bool, Error> {
        let (connection, tokens) = Self::disconnect_credentials(store, id).await?;
        let Some(tokens) = tokens else {
            return Ok(false);
        };
        let Ok(client) = Self::new() else {
            return Ok(false);
        };
        Ok(client
            .revoke_tokens(&connection.registration.client_id, &tokens)
            .await)
    }

    /// Local disconnect is final even when the provider cannot confirm revoke.
    /// False must be shown as unconfirmed revocation, never as success.
    pub async fn disconnect(&self, store: Store, id: Uuid) -> Result<bool, Error> {
        let (connection, tokens) = Self::disconnect_credentials(store, id).await?;
        match tokens {
            Some(tokens) => Ok(self
                .revoke_tokens(&connection.registration.client_id, &tokens)
                .await),
            None => Ok(false),
        }
    }
}

async fn body(response: reqwest::Response) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|bytes| bytes > BODY_LIMIT as u64)
    {
        return Err(Error::InvalidResponse);
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::Unavailable)?;
        if chunk.len() > BODY_LIMIT.saturating_sub(bytes.len()) {
            return Err(Error::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rsa::{pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
    use serde_json::{Value, json};
    use std::sync::LazyLock;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    static KEY: LazyLock<rsa::RsaPrivateKey> =
        LazyLock::new(|| rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap());

    fn jwks() -> Value {
        json!({"keys":[{"kty":"RSA","alg":"RS256","use":"sig","kid":"synthetic-test",
            "n":URL_SAFE_NO_PAD.encode(KEY.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(KEY.e().to_bytes_be())}]})
    }

    fn id_token(issuer: &str, client_id: &str, nonce: &str) -> String {
        let encoding =
            jsonwebtoken::EncodingKey::from_rsa_der(KEY.to_pkcs1_der().unwrap().as_bytes());
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("synthetic-test".into());
        jsonwebtoken::encode(
            &header,
            &json!({"iss":issuer,"sub":"synthetic-provider-user",
            "aud":client_id,"nonce":nonce,"exp":now().unwrap()+3600}),
            &encoding,
        )
        .unwrap()
    }

    async fn client(server: &MockServer, timeout: Duration) -> Arc<Client> {
        let origin = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer":origin,"jwks_uri":format!("{origin}/jwks"),
                "revocation_endpoint":format!("{origin}/revoke")
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks()))
            .mount(server)
            .await;
        Arc::new(Client {
            http: reqwest::Client::builder()
                .no_proxy()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .build()
                .unwrap(),
            endpoints: Endpoints {
                api_base: format!("{origin}/v1"),
                issuer: origin.clone(),
                token: format!("{origin}/token"),
                discovery: format!("{origin}/.well-known/openid-configuration"),
            },
            metadata: Mutex::new(None),
        })
    }

    fn store() -> (
        tempfile::TempDir,
        astra_credentials::CredentialsDirOverrideGuard,
        Store,
        Uuid,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let guard = astra_credentials::set_test_credentials_dir(directory.path().into());
        let scope = astra_credentials::LocalModelScope::for_owner(
            "https://astra.example.invalid",
            "synthetic-owner",
        )
        .unwrap();
        let store = Store::new(&scope);
        let id = Uuid::new_v4();
        let host_id = store.host_id().unwrap();
        store
            .authorize(
                id,
                None,
                "Synthetic".into(),
                Registration {
                    client_id: "synthetic-issued-client".into(),
                    subject: "synthetic-provider-user".into(),
                    host_id,
                },
                Tokens::new(
                    "synthetic-old-access".into(),
                    "synthetic-old-refresh".into(),
                    now().unwrap() - 1,
                )
                .unwrap(),
                true,
            )
            .unwrap();
        (directory, guard, store, id)
    }

    fn rotated() -> Value {
        json!({"token_type":"Bearer","access_token":"synthetic-new-access",
            "refresh_token":"synthetic-new-refresh","expires_in":3600})
    }

    #[tokio::test]
    async fn simultaneous_terminals_send_one_rotation_and_use_committed_replacement() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(rotated())
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let client = client.clone();
            let store = store.clone();
            tasks.push(tokio::spawn(
                async move { client.credentials(store, id).await },
            ));
        }
        for task in tasks {
            let (connection, tokens) = task.await.unwrap().unwrap();
            assert_eq!(connection.token_version, 2);
            assert_eq!(tokens.refresh_token(), "synthetic-new-refresh");
        }
        let requests = server.received_requests().await.unwrap();
        let request = requests.iter().find(|r| r.url.path() == "/token").unwrap();
        let form: std::collections::BTreeMap<_, _> = reqwest::Url::parse(&format!(
            "http://fixture.invalid/?{}",
            String::from_utf8_lossy(&request.body)
        ))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "synthetic-old-refresh");
        assert_eq!(form["resource"], chatgpt::API_BASE);
        assert!(!form.contains_key("scope"));
    }

    #[tokio::test]
    async fn late_refresh_never_revokes_or_overwrites_a_new_authorization() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let signal = reached.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(200)
                    .set_body_json(rotated())
                    .set_delay(Duration::from_millis(150))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let previous = store.list().unwrap().remove(0);
        let pending = {
            let client = client.clone();
            let store = store.clone();
            tokio::spawn(async move { client.credentials(store, id).await })
        };
        tokio::time::timeout(Duration::from_secs(2), reached.notified())
            .await
            .unwrap();
        store
            .authorize(
                id,
                Some(previous.authorization_revision),
                "new winner".into(),
                previous.registration,
                Tokens::new(
                    "synthetic-winner-access".into(),
                    "synthetic-winner-refresh".into(),
                    now().unwrap() + 3600,
                )
                .unwrap(),
                true,
            )
            .unwrap();
        assert!(matches!(
            pending.await.unwrap(),
            Err(Error::Credentials(chatgpt::Error::Changed))
        ));
        let (winner, tokens) = store.token_snapshot(id).unwrap();
        assert_eq!(winner.authorization_revision, 2);
        assert_eq!(tokens.refresh_token(), "synthetic-winner-refresh");
    }

    #[tokio::test]
    async fn lost_rotation_response_is_unknown_and_old_token_is_never_retried() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_millis(60)).await;
        // Populate discovery outside the intentionally short exchange deadline.
        client.metadata().await.unwrap();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(rotated())
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client.credentials(store.clone(), id).await.unwrap_err(),
            Error::Unavailable
        );
        assert_eq!(
            client.credentials(store.clone(), id).await.unwrap_err(),
            Error::Credentials(chatgpt::Error::RefreshUnknown)
        );
        assert_eq!(
            store.list().unwrap()[0].credential_state,
            CredentialState::RefreshUnknown
        );
    }

    #[tokio::test]
    async fn cancelling_waiter_does_not_abandon_rotation_settlement() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        client.metadata().await.unwrap();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(rotated())
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let worker = {
            let client = client.clone();
            let store = store.clone();
            tokio::spawn(async move { client.credentials(store, id).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/token")
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        worker.abort();
        let _ = worker.await;
        let (_, tokens) = client.credentials(store, id).await.unwrap();
        assert_eq!(tokens.refresh_token(), "synthetic-new-refresh");
    }

    #[tokio::test]
    async fn authoritative_invalid_grant_clears_tokens_without_leaking_provider_body() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"error":"invalid_grant","error_description":"synthetic-secret-canary"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let error = client.credentials(store.clone(), id).await.unwrap_err();
        assert_eq!(error, Error::Credentials(chatgpt::Error::Reauthorize));
        assert!(!format!("{error:?} {error}").contains("synthetic-secret-canary"));
        assert_eq!(
            store.list().unwrap()[0].credential_state,
            CredentialState::AuthorizationRequired
        );
    }

    #[tokio::test]
    async fn explicit_disconnect_clears_locally_when_remote_revoke_is_unconfirmed() {
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        assert!(!client.disconnect(store.clone(), id).await.unwrap());
        assert_eq!(
            store.list().unwrap()[0].credential_state,
            CredentialState::Disconnected
        );
        assert!(store.token_snapshot(id).is_err());
    }

    #[tokio::test]
    async fn code_exchange_validates_identity_and_retains_declined_plan_registration() {
        let (_directory, _guard, store, _) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        let request = client
            .begin(
                store.host_id().unwrap(),
                None,
                "http://127.0.0.1:41001/auth/callback".into(),
            )
            .unwrap();
        Mock::given(method("POST")).and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type":"Bearer","access_token":"synthetic-access","refresh_token":"synthetic-refresh",
                "expires_in":3600,"scope":"openid profile offline_access",
                "id_token":id_token(&server.uri(),"issued-client",&request.nonce)
            }))).expect(1).mount(&server).await;
        let callback = format!(
            "state={}&code=synthetic-code&client_id=issued-client",
            request.state
        );
        let grant = client.exchange(&request, &callback, None).await.unwrap();
        assert!(!format!("{grant:?}").contains("synthetic"));
        let connection = grant
            .save(&store, Uuid::new_v4(), None, "Personal".into())
            .unwrap();
        assert_eq!(connection.credential_state, CredentialState::PlanDisabled);
        assert!(!connection.enabled);
        assert_eq!(connection.registration.client_id, "issued-client");
    }

    #[tokio::test]
    async fn unknown_identity_kid_refreshes_metadata_without_repeating_the_code_exchange() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_directory, _guard, store, _) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        let visits = Arc::new(AtomicUsize::new(0));
        let calls = visits.clone();
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(move |_: &wiremock::Request| {
                let mut value = jwks();
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    value["keys"][0]["kid"] = json!("retired-key");
                }
                ResponseTemplate::new(200).set_body_json(value)
            })
            .with_priority(1)
            .expect(2)
            .mount(&server)
            .await;
        client.metadata().await.unwrap();
        let request = client
            .begin(
                store.host_id().unwrap(),
                None,
                "http://127.0.0.1:41001/auth/callback".into(),
            )
            .unwrap();
        Mock::given(method("POST")).and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type":"Bearer","access_token":"synthetic-access","refresh_token":"synthetic-refresh",
                "expires_in":3600,"scope":chatgpt::PLAN_SCOPE,"id_token":id_token(&server.uri(),"issued-client",&request.nonce)
            }))).expect(1).mount(&server).await;
        let callback = format!(
            "state={}&code=synthetic-code&client_id=issued-client",
            request.state
        );
        let grant = client.exchange(&request, &callback, None).await.unwrap();
        assert!(grant.plan_authorized);
        assert_eq!(visits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalid_identity_never_publishes_and_reports_abandoned_grant_cleanup() {
        let (_directory, _guard, store, _) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        let before = store.list().unwrap().len();
        let request = client
            .begin(
                store.host_id().unwrap(),
                None,
                "http://127.0.0.1:41001/auth/callback".into(),
            )
            .unwrap();
        Mock::given(method("POST")).and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type":"Bearer","access_token":"synthetic-access","refresh_token":"synthetic-refresh",
                "expires_in":3600,"scope":chatgpt::PLAN_SCOPE,"id_token":id_token(&server.uri(),"issued-client","wrong-nonce")
            }))).expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let callback = format!(
            "state={}&code=synthetic-code&client_id=issued-client",
            request.state
        );
        assert!(matches!(
            client.exchange(&request, &callback, None).await,
            Err(Error::GrantRejected {
                revocation_confirmed: true
            })
        ));
        assert_eq!(store.list().unwrap().len(), before);
    }

    #[tokio::test]
    async fn device_catalog_and_probe_use_the_fixed_responses_protocol() {
        use wiremock::matchers::{body_partial_json, header};
        let (_directory, _guard, store, id) = store();
        let server = MockServer::start().await;
        let client = client(&server, Duration::from_secs(2)).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rotated()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer synthetic-new-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"models":[{"slug":"synthetic-model","visibility":"list",
                "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        let completed = json!({"type":"response.completed","response":{"id":"synthetic-response","status":"completed",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}});
        Mock::given(method("POST")).and(path("/v1/responses")).and(header("authorization", "Bearer synthetic-new-access"))
            .and(body_partial_json(json!({"model":"synthetic-model","stream":true,"store":false,"max_output_tokens":64})))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("event: response.completed\ndata: {completed}\n\n"))).expect(1).mount(&server).await;
        assert_eq!(
            client.catalog(store.clone(), id).await.unwrap()[0].slug,
            "synthetic-model"
        );
        assert_eq!(
            client
                .probe(store, id, "synthetic-model", 64)
                .await
                .unwrap(),
            ["low", "high"]
        );
    }

    #[test]
    fn device_provider_failure_does_not_misclassify_moi_login() {
        assert_eq!(
            provider_status(reqwest::StatusCode::FORBIDDEN),
            Error::ProviderForbidden
        );
        assert_eq!(
            provider_status(reqwest::StatusCode::UNAUTHORIZED),
            Error::AuthorizationRequired
        );
        assert_eq!(
            provider_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            Error::RateLimited
        );
        assert_eq!(
            provider_status(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            Error::Unavailable
        );
    }

    #[test]
    fn production_endpoints_are_fixed_and_discovery_cannot_redirect_credentials() {
        let client = Client::new().unwrap();
        assert_eq!(client.endpoints.token, protocol::TOKEN_ENDPOINT);
        for endpoint in [
            "http://auth.openai.com/jwks",
            "https://evil.example.invalid/jwks",
            "https://user:secret@auth.openai.com/jwks",
            "https://auth.openai.com/jwks?token=secret",
        ] {
            assert!(client.metadata_endpoint(endpoint).is_err());
        }
    }
}
