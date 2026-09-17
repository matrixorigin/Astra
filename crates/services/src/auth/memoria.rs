//! Application-scoped Memoria verification and credential lifecycle.
use super::{AuthHttpError, AuthTokenRecord, DatabaseAuthService, sha256_hex};
use crate::FernetTokenEncryptor;
use astra_core::{MemoriaSettings, SharedPool, error_response, internal_error};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub const ACCESS_TTL_SECONDS: u32 = 900;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAccess {
    None,
    ReadOnly,
    ReadWrite,
}
impl MemoryAccess {
    /// User-facing remediation shared by explicit tools and HTTP memory routes.
    pub fn denial_message(self, write: bool) -> Option<&'static str> {
        match (self, write) {
            (Self::None, _) => Some(
                "Memory sharing with Astra is disabled. Open Memoria Settings → Connected apps → Astra Cloud → Memory sharing settings and choose read-only or read/write access. Signing in to Astra does not enable memory sharing automatically.",
            ),
            (Self::ReadOnly, true) => Some(
                "Memory sharing is read-only. Reading memories is allowed, but saving or modifying them requires read/write access. Open Memoria Settings → Connected apps → Astra Cloud → Memory sharing settings to change access.",
            ),
            _ => None,
        }
    }
    pub fn allows(self, write: bool) -> bool {
        self == Self::ReadWrite || (!write && self == Self::ReadOnly)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }
}
pub fn memory_access_for_scopes(scopes: &[String]) -> Option<MemoryAccess> {
    let mut scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    scopes.sort_unstable();
    scopes.dedup();
    match scopes.as_slice() {
        ["identity:read"] => Some(MemoryAccess::None),
        ["identity:read", "memory:read"] => Some(MemoryAccess::ReadOnly),
        ["identity:read", "memory:read", "memory:write"] => Some(MemoryAccess::ReadWrite),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct MemoriaProvider {
    pub base_url: String,
    pub issuer: String,
    pub provider_id: String,
    pub web_url: Option<String>,
    legacy_issuer: Option<String>,
}
fn normalize_url(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Memoria URL must be absolute".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Memoria URL must be HTTP(S), without credentials, query or fragment".into());
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn is_loopback_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl MemoriaProvider {
    pub fn new(settings: &MemoriaSettings) -> Result<Self, String> {
        let base_url = normalize_url(&settings.base_url)?;
        let issuer = normalize_url(settings.issuer.as_deref().unwrap_or(&base_url))?;
        let web_url = settings.web_url.as_deref().map(normalize_url).transpose()?;
        if let Some(web) = &web_url {
            let url = reqwest::Url::parse(web).map_err(|e| e.to_string())?;
            if url.scheme() != "https" && !is_loopback_url(web) {
                return Err("MEMORIA_WEB_URL requires HTTPS except on loopback".into());
            }
        }
        Ok(Self {
            provider_id: format!("memoria:{}", sha256_hex(&issuer)),
            legacy_issuer: settings
                .legacy_issuer
                .as_deref()
                .map(normalize_url)
                .transpose()?,
            base_url,
            issuer,
            web_url,
        })
    }
    async fn verify(&self, key: &str) -> Result<VerifiedMemoriaIdentity, AuthHttpError> {
        if key.trim().is_empty() || key.len() > 4096 {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Invalid Memoria connection key",
            ));
        }
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10));
        // A local development/test endpoint must not be sent through a
        // process-wide HTTP proxy. Keep proxy support for real remote
        // deployments, where the operator may require an egress proxy.
        if is_loopback_url(&self.base_url) {
            client = client.no_proxy();
        }
        let response = client
            .build()
            .map_err(|_| unavailable())?
            .get(format!("{}/auth/whoami", self.base_url))
            .bearer_auth(key)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(reconnect());
        }
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let value: Value = response.json().await.map_err(|_| unavailable())?;
        let user_id = value["user_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 128)
            .ok_or_else(reconnect)?;
        let key_id = value["key_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(reconnect)?;
        let scopes: Vec<String> =
            serde_json::from_value(value["granted_scopes"].clone()).map_err(|_| reconnect())?;
        let access = memory_access_for_scopes(&scopes).ok_or_else(reconnect)?;
        if value["is_active"] != true
            || value["is_master"] != false
            || value["scope"]["type"] != "personal"
            || value["scope"]["id"] != user_id
            || value["api_version"] != "1"
            || !value["capabilities"].as_array().is_some_and(|c| {
                c.iter().any(|v| v == "api_key_scopes")
                    && c.iter().any(|v| v == "memory_filters_v1")
            })
        {
            return Err(reconnect());
        }
        Ok(VerifiedMemoriaIdentity {
            memoria_user_id: user_id.to_string(),
            key_id: key_id.to_string(),
            memory_access: access,
            granted_scopes: scopes,
            issuer: self.issuer.clone(),
            connection_generation: None,
        })
    }
}
#[derive(Clone, Deserialize, Serialize)]
struct VerifiedMemoriaIdentity {
    memoria_user_id: String,
    key_id: String,
    memory_access: MemoryAccess,
    granted_scopes: Vec<String>,
    issuer: String,
    /// Astra-owned lifecycle nonce, never supplied by the upstream verifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_generation: Option<String>,
}
pub struct MemoriaLogin {
    pub tokens: AuthTokenRecord,
    pub memory_access: MemoryAccess,
    pub granted_scopes: Vec<String>,
}
/// Deliberately not Debug/Serialize: plaintext credentials must not be logged.
pub struct MemoriaCredential {
    pub key: String,
    pub owner: String,
    pub generation: String,
    pub access: MemoryAccess,
    connection_generation: Option<String>,
}

/// Current runtime authority for one Astra account.
///
/// `UnboundLocal` is deliberately narrower than an absent credential: only an
/// active password account with no retained Memoria identity may receive the
/// explicitly enabled self-hosted fallback. Disconnect and account lifecycle
/// changes therefore cannot turn a previously scoped runtime into a new
/// deployment-master grant.
pub enum MemoriaCredentialResolution<T> {
    Scoped(T),
    UnboundLocal,
    Denied,
}

#[derive(PartialEq, Eq)]
pub(super) struct ReauthenticationBinding {
    provider_id: String,
    owner: String,
    generation: String,
    connection_generation: Option<String>,
}

pub(super) fn reauthentication_proof_hash(
    proof: &str,
    binding: Option<&ReauthenticationBinding>,
) -> String {
    match binding {
        Some(binding) => sha256_hex(
            &serde_json::json!([
                proof,
                binding.provider_id,
                binding.owner,
                binding.generation,
                binding.connection_generation
            ])
            .to_string(),
        ),
        None => sha256_hex(proof),
    }
}
#[derive(Clone)]
pub struct MemoriaCredentialResolver {
    pub provider: MemoriaProvider,
    pool: SharedPool,
    encryptor: FernetTokenEncryptor,
}
impl MemoriaCredentialResolver {
    pub fn new(
        provider: MemoriaProvider,
        pool: SharedPool,
        encryptor: FernetTokenEncryptor,
    ) -> Self {
        Self {
            provider,
            pool,
            encryptor,
        }
    }
    fn token_id(&self, user: &str) -> String {
        sha256_hex(&format!(
            "memoria-binding\0{}\0{user}",
            self.provider.provider_id
        ))
    }
    fn decode_credential(
        &self,
        ciphertext: Option<&str>,
        metadata: Option<&str>,
    ) -> Result<MemoriaCredential, String> {
        let identity: VerifiedMemoriaIdentity = serde_json::from_str(metadata.unwrap_or(""))
            .map_err(|_| "Invalid Memoria binding metadata".to_string())?;
        if identity.issuer != self.provider.issuer {
            return Err("Memoria binding issuer mismatch".into());
        }
        let key = self
            .encryptor
            .decrypt(ciphertext.ok_or("Missing Memoria credential")?)
            .map_err(|_| "Memoria credential decryption failed".to_string())?;
        Ok(MemoriaCredential {
            key,
            owner: identity.memoria_user_id,
            generation: identity.key_id,
            access: identity.memory_access,
            connection_generation: identity.connection_generation,
        })
    }
    pub async fn resolve(&self, user: &str) -> Result<Option<MemoriaCredential>, String> {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT encrypted_value, CAST(metadata AS CHAR) FROM auth_tokens WHERE token_id = ? AND type = 'memoria_connection' AND provider = 'memoria' AND scope_user_id = ? AND is_active = 1 AND EXISTS (SELECT 1 FROM auth_users WHERE user_id = auth_tokens.scope_user_id AND is_active = 1)")
            .bind(self.token_id(user)).bind(user).fetch_optional(self.pool.get()).await
            .map_err(|_| "Memoria credential lookup failed".to_string())?;
        let Some((ciphertext, metadata)) = row else {
            return Ok(None);
        };
        self.decode_credential(ciphertext.as_deref(), metadata.as_deref())
            .map(Some)
    }

    /// Resolve both the current credential and whether an absent credential
    /// represents an active, genuinely local account that is eligible for the
    /// opt-in self-hosted fallback.
    pub async fn resolve_runtime(
        &self,
        user: &str,
    ) -> Result<MemoriaCredentialResolution<MemoriaCredential>, String> {
        if let Some(credential) = self.resolve(user).await? {
            return Ok(MemoriaCredentialResolution::Scoped(credential));
        }
        let eligible: Option<String> = sqlx::query_scalar(
            "SELECT u.user_id FROM auth_users u \
             WHERE u.user_id = ? AND u.is_active = 1 AND u.password_hash <> '' \
             AND NOT EXISTS (SELECT 1 FROM auth_tokens t WHERE t.type = 'memoria_connection' AND t.provider = 'memoria' AND t.scope_user_id = u.user_id) \
             AND NOT EXISTS (SELECT 1 FROM auth_external_identities e WHERE e.astra_user_id = u.user_id AND e.provider_id LIKE 'memoria:%') \
             AND NOT EXISTS (SELECT 1 FROM auth_memoria_identities l WHERE l.astra_user_id = u.user_id) \
             LIMIT 1",
        )
        .bind(user)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|_| "Memoria runtime fallback eligibility lookup failed".to_string())?;
        Ok(if eligible.is_some() {
            MemoriaCredentialResolution::UnboundLocal
        } else {
            MemoriaCredentialResolution::Denied
        })
    }
}
impl DatabaseAuthService {
    pub(super) async fn reauthentication_binding(
        &self,
        pool: &sqlx::MySqlPool,
        user: &str,
    ) -> Result<Option<ReauthenticationBinding>, AuthHttpError> {
        let Some(owner) = self.memoria_owner(pool, user).await? else {
            return Ok(None);
        };
        let resolver = self.credential_resolver().ok_or_else(reconnect)?;
        let credential = resolver
            .resolve(user)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(reconnect)?;
        let verified = resolver.provider.verify(&credential.key).await?;
        if verified.memoria_user_id != owner
            || credential.owner != owner
            || verified.key_id != credential.generation
        {
            return Err(reconnect());
        }
        Ok(Some(ReauthenticationBinding {
            provider_id: resolver.provider.provider_id,
            owner,
            generation: credential.generation,
            connection_generation: credential.connection_generation,
        }))
    }

    pub(super) async fn verify_memoria_step_up(
        &self,
        binding: &ReauthenticationBinding,
        proof: &str,
        purpose: super::ReauthenticationPurpose,
    ) -> Result<(), AuthHttpError> {
        if !proof.starts_with("msu_")
            || proof.len() != 68
            || !proof[4..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Fresh account verification is required",
            ));
        }
        let provider = self.memoria_provider.as_ref().ok_or_else(reconnect)?;
        let web = provider.web_url.as_ref().ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Account reauthentication website is not configured",
            )
        })?;
        // Only the composition-time trusted website may attest fresh authentication.
        // A caller cannot supply a verifier URL or a different identity authority.
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10));
        if is_loopback_url(web) {
            client = client.no_proxy();
        }
        let mut response = client
            .build()
            .map_err(|_| unavailable())?
            .post(format!("{web}/api/auth/astra/reauthentication/consume"))
            .json(&serde_json::json!({"proof":proof,"subject":binding.owner,"key_id":binding.generation,"purpose":purpose}))
            .send().await.map_err(|_| unavailable())?;
        if matches!(response.status().as_u16(), 400 | 401 | 403 | 409) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Account verification is invalid, expired or already used",
            ));
        }
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + chunk.len() > 4096 {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let now = chrono::Utc::now().timestamp();
        let authenticated_at = value["authenticated_at"].as_i64().ok_or_else(reconnect)?;
        let expires_at = value["expires_at"].as_i64().ok_or_else(reconnect)?;
        if value["subject"] != binding.owner
            || value["key_id"] != binding.generation
            || value["purpose"] != purpose.as_str()
            || authenticated_at > now + 5
            || now - authenticated_at > 120
            || expires_at <= now
            || expires_at > authenticated_at + 120
        {
            return Err(reconnect());
        }
        Ok(())
    }

    pub fn with_memoria_settings(mut self, settings: &MemoriaSettings) -> Result<Self, String> {
        self.memoria_provider = Some(MemoriaProvider::new(settings)?);
        Ok(self)
    }
    pub fn with_memoria_base_url(self, base_url: String) -> Self {
        self.with_memoria_settings(&MemoriaSettings {
            base_url,
            master_key: None,
            self_hosted_master_access: false,
            issuer: None,
            web_url: None,
            legacy_issuer: None,
        })
        .expect("valid Memoria URL")
    }
    pub(super) fn credential_resolver(&self) -> Option<MemoriaCredentialResolver> {
        Some(MemoriaCredentialResolver::new(
            self.memoria_provider.clone()?,
            self.pool.clone()?,
            self.encryptor.as_ref()?.clone(),
        ))
    }
    pub(super) async fn memoria_owner(
        &self,
        pool: &sqlx::MySqlPool,
        user: &str,
    ) -> Result<Option<String>, AuthHttpError> {
        let identity: Option<(String, String)> = sqlx::query_as(
            "SELECT provider_id, external_subject FROM auth_external_identities WHERE astra_user_id = ? AND provider_id LIKE 'memoria:%' LIMIT 1")
            .bind(user).fetch_optional(pool).await.map_err(internal_error)?;
        if let Some((provider, subject)) = identity {
            if self
                .memoria_provider
                .as_ref()
                .is_none_or(|p| p.provider_id != provider)
            {
                return Err(reconnect());
            }
            return Ok(Some(subject));
        }
        let legacy: Option<String> = sqlx::query_scalar(
            "SELECT memoria_user_id FROM auth_memoria_identities WHERE astra_user_id = ? LIMIT 1",
        )
        .bind(user)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
        if legacy.is_some() {
            return Err(reconnect());
        }
        Ok(None)
    }
    pub(super) async fn revalidate_memoria_connection(
        &self,
        _pool: &sqlx::MySqlPool,
        user: &str,
        owner: &str,
    ) -> Result<(), AuthHttpError> {
        let resolver = self.credential_resolver().ok_or_else(unavailable)?;
        let credential = resolver
            .resolve(user)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(reconnect)?;
        let identity = resolver.provider.verify(&credential.key).await?;
        if identity.memoria_user_id != owner || identity.key_id != credential.generation {
            return Err(reconnect());
        }
        Ok(())
    }
    pub(super) async fn memoria_login(&self, key: &str) -> Result<MemoriaLogin, AuthHttpError> {
        let resolver = self.credential_resolver().ok_or_else(unavailable)?;
        let identity = resolver.provider.verify(key).await?;
        let ciphertext = resolver.encryptor.encrypt(key).map_err(internal_error)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.ensure_default_roles(&pool)
            .await
            .map_err(internal_error)?;
        // Retry aborted identity insert / serialization transactions, never only
        // the credential phase. No durable session exists on a failed attempt.
        for attempt in 0..3 {
            match self
                .persist_memoria_login(&pool, &resolver, &identity, &ciphertext, key)
                .await
            {
                Ok(tokens) => {
                    return Ok(MemoriaLogin {
                        tokens,
                        memory_access: identity.memory_access,
                        granted_scopes: identity.granted_scopes,
                    });
                }
                Err(error) if attempt < 2 && error.0 == StatusCode::INTERNAL_SERVER_ERROR => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }
    async fn persist_memoria_login(
        &self,
        pool: &sqlx::MySqlPool,
        resolver: &MemoriaCredentialResolver,
        identity: &VerifiedMemoriaIdentity,
        ciphertext: &str,
        key: &str,
    ) -> Result<AuthTokenRecord, AuthHttpError> {
        let mut tx = pool.begin().await.map_err(internal_error)?;
        let legacy: Option<String> = sqlx::query_scalar(
            "SELECT astra_user_id FROM auth_memoria_identities WHERE memoria_user_id = ? LIMIT 1",
        )
        .bind(&identity.memoria_user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal_error)?;
        if legacy.is_some()
            && resolver.provider.legacy_issuer.as_deref() != Some(&resolver.provider.issuer)
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Legacy Memoria identity requires administrator-configured MEMORIA_LEGACY_ISSUER migration",
            ));
        }
        let user = self
            .resolve_verified_provider_identity(
                &mut tx,
                &resolver.provider.provider_id,
                &identity.memoria_user_id,
                legacy.as_deref(),
            )
            .await?;
        // Reject credentials revoked while waiting on a concurrent link/unlink.
        let fresh = resolver.provider.verify(key).await?;
        if fresh.memoria_user_id != identity.memoria_user_id
            || fresh.key_id != identity.key_id
            || fresh.memory_access != identity.memory_access
        {
            return Err(reconnect());
        }
        // The canonical account lock serializes login with disconnect. Preserve
        // the lifecycle only for an uninterrupted binding to the same key.
        // Upstream key IDs can be reused after disconnect; this nonce cannot.
        let previous: Option<String> = sqlx::query_scalar("SELECT CAST(metadata AS CHAR) FROM auth_tokens WHERE token_id = ? AND type = 'memoria_connection' AND provider = 'memoria' AND scope_user_id = ? AND is_active = 1")
            .bind(resolver.token_id(&user.user_id)).bind(&user.user_id)
            .fetch_optional(&mut *tx).await.map_err(internal_error)?;
        let previous =
            previous.and_then(|value| serde_json::from_str::<VerifiedMemoriaIdentity>(&value).ok());
        let mut stored_identity = identity.clone();
        stored_identity.connection_generation = Some(
            previous
                .filter(|old| {
                    old.issuer == identity.issuer
                        && old.memoria_user_id == identity.memoria_user_id
                        && old.key_id == identity.key_id
                })
                .and_then(|old| old.connection_generation)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        );
        sqlx::query("DELETE FROM auth_tokens WHERE type = 'memoria_connection' AND provider = 'memoria' AND scope_user_id = ?")
            .bind(&user.user_id).execute(&mut *tx).await.map_err(internal_error)?;
        sqlx::query("INSERT INTO auth_tokens (token_id,type,provider,encrypted_value,is_active,scope_user_id,metadata) VALUES (?, 'memoria_connection', 'memoria', ?, 1, ?, ?)")
            .bind(resolver.token_id(&user.user_id)).bind(ciphertext).bind(&user.user_id)
            .bind(serde_json::to_string(&stored_identity).map_err(internal_error)?).execute(&mut *tx).await.map_err(internal_error)?;
        if legacy.is_some() {
            sqlx::query("DELETE FROM auth_memoria_identities WHERE astra_user_id = ?")
                .bind(&user.user_id)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
            sqlx::query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE user_id = ?")
                .bind(&user.user_id)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
        }
        let session = uuid::Uuid::new_v4().to_string();
        let origin = format!("verified:{}", resolver.provider.provider_id);
        let access_token = self
            .create_access_token(&user.user_id, &user.username, &session, &origin)
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(&user.user_id, &session, &origin)
            .map_err(internal_error)?;
        sqlx::query("INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,is_revoked) VALUES (?, ?, ?, ?, ?, 0)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&user.user_id).bind(&session).bind(sha256_hex(&refresh_token))
            .bind(self.refresh_token_expires_at_string(chrono::Utc::now())).execute(&mut *tx).await.map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        Ok(AuthTokenRecord {
            user_id: user.user_id,
            access_token,
            refresh_token,
            token_type: "bearer".into(),
            expires_in: self
                .access_token_expires_in_seconds()
                .min(ACCESS_TTL_SECONDS),
        })
    }
    pub(super) async fn memoria_disconnect(&self, user: &str) -> Result<(), AuthHttpError> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut tx = pool.begin().await.map_err(internal_error)?;
        sqlx::query("SELECT user_id FROM auth_users WHERE user_id = ? FOR UPDATE")
            .bind(user)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;
        sqlx::query("DELETE FROM auth_tokens WHERE type = 'memoria_connection' AND provider = 'memoria' AND scope_user_id = ?")
            .bind(user).execute(&mut *tx).await.map_err(internal_error)?;
        sqlx::query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE user_id = ?")
            .bind(user)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        sqlx::query("DELETE FROM auth_reauthentication_proofs WHERE user_id = ?")
            .bind(user)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        Ok(())
    }
}
fn reconnect() -> AuthHttpError {
    error_response(
        StatusCode::UNAUTHORIZED,
        "Memoria connection expired or revoked; sign in again",
    )
}
fn unavailable() -> AuthHttpError {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Memoria identity verification is unavailable; retry later",
    )
}

#[cfg(test)]
async fn verify_connection(
    base_url: &str,
    key: &str,
    owner: &str,
    key_id: &str,
) -> Result<(), AuthHttpError> {
    let provider = MemoriaProvider::new(&MemoriaSettings {
        base_url: base_url.into(),
        master_key: None,
        self_hosted_master_access: false,
        issuer: None,
        web_url: None,
        legacy_issuer: None,
    })
    .unwrap();
    let identity = provider.verify(key).await?;
    if identity.memoria_user_id != owner || identity.key_id != key_id {
        return Err(reconnect());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consent_remediation_preserves_sign_in_and_memory_sharing_separation() {
        for write in [false, true] {
            let message = MemoryAccess::None.denial_message(write).unwrap();
            assert!(message.contains("Memory sharing with Astra is disabled"));
            assert!(message.contains("Memory sharing settings"));
            assert!(!message.contains("/login"));
            assert!(!message.contains("MEMORIA_MASTER_KEY"));
            assert!(MemoryAccess::ReadWrite.denial_message(write).is_none());
        }
        assert!(MemoryAccess::ReadOnly.denial_message(false).is_none());
        let message = MemoryAccess::ReadOnly.denial_message(true).unwrap();
        assert!(message.contains("read-only"));
        assert!(message.contains("requires read/write access"));
    }
    #[test]
    fn step_up_proof_hash_binds_issuer_subject_and_generation_without_ambiguous_fields() {
        let binding = ReauthenticationBinding {
            provider_id: "memoria:one".into(),
            owner: "owner".into(),
            generation: "key".into(),
            connection_generation: Some("lifecycle-1".into()),
        };
        let expected = reauthentication_proof_hash("rp_test", Some(&binding));
        for other in [
            ReauthenticationBinding {
                provider_id: "memoria:two".into(),
                owner: "owner".into(),
                generation: "key".into(),
                connection_generation: Some("lifecycle-1".into()),
            },
            ReauthenticationBinding {
                provider_id: "memoria:one".into(),
                owner: "other".into(),
                generation: "key".into(),
                connection_generation: Some("lifecycle-1".into()),
            },
            ReauthenticationBinding {
                provider_id: "memoria:one".into(),
                owner: "owner".into(),
                generation: "rotated".into(),
                connection_generation: Some("lifecycle-1".into()),
            },
            ReauthenticationBinding {
                provider_id: "memoria:one".into(),
                owner: "owner".into(),
                generation: "key".into(),
                connection_generation: Some("lifecycle-2".into()),
            },
            ReauthenticationBinding {
                provider_id: "memoria:one".into(),
                owner: "owner".into(),
                generation: "key".into(),
                connection_generation: None,
            },
        ] {
            assert_ne!(
                expected,
                reauthentication_proof_hash("rp_test", Some(&other))
            );
        }
        assert_ne!(expected, reauthentication_proof_hash("rp_test", None));
        let left = ReauthenticationBinding {
            provider_id: "p".into(),
            owner: "x\0y".into(),
            generation: "z".into(),
            connection_generation: None,
        };
        let right = ReauthenticationBinding {
            provider_id: "p".into(),
            owner: "x".into(),
            generation: "y\0z".into(),
            connection_generation: None,
        };
        assert_ne!(
            reauthentication_proof_hash("rp_test", Some(&left)),
            reauthentication_proof_hash("rp_test", Some(&right))
        );
    }
    use axum::{Json, Router, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    async fn wait_for_fixture(url: &str) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("fixture HTTP client");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if client
                .get(format!("{url}/auth/whoami"))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "local Memoria HTTP fixture did not become ready"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn refresh_identity_rechecks_revocation_owner_and_outages() {
        let revoked = Arc::new(AtomicBool::new(false));
        let flag = revoked.clone();
        let app = Router::new().route("/auth/whoami", get(move || {
            let flag = flag.clone();
            async move {
                (if flag.load(Ordering::SeqCst) { StatusCode::UNAUTHORIZED } else { StatusCode::OK }, Json(serde_json::json!({
                    "user_id":"owner", "key_id":"key", "is_active":true, "is_master":false,
                    "scope":{"type":"personal","id":"owner"}, "api_version":"1",
                    "capabilities":["api_key_scopes","memory_filters_v1"], "granted_scopes":["identity:read"]
                })))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        wait_for_fixture(&url).await;
        let verified = verify_connection(&url, "secret", "owner", "key").await;
        assert!(
            verified.is_ok(),
            "fixture verification failed: {verified:?}"
        );
        assert_eq!(
            verify_connection(&url, "secret", "other-owner", "key")
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            verify_connection(&url, "secret", "owner", "other-key")
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        revoked.store(true, Ordering::SeqCst);
        assert_eq!(
            verify_connection(&url, "secret", "owner", "key")
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        task.abort();
        let error = verify_connection(&url, "secret", "owner", "key")
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!error.1.detail.contains("secret"));
    }
}

#[cfg(test)]
mod provider_contract_tests {
    use super::*;
    fn settings(base: &str, web: Option<&str>) -> MemoriaSettings {
        MemoriaSettings {
            base_url: base.into(),
            master_key: None,
            self_hosted_master_access: false,
            issuer: None,
            web_url: web.map(str::to_string),
            legacy_issuer: None,
        }
    }
    #[test]
    fn provider_identity_is_canonical_and_namespaced() {
        let a = MemoriaProvider::new(&settings("https://A.example:443/", None)).unwrap();
        let same = MemoriaProvider::new(&settings("https://a.example", None)).unwrap();
        let other = MemoriaProvider::new(&settings("https://b.example", None)).unwrap();
        assert_eq!(a.provider_id, same.provider_id);
        assert_ne!(a.provider_id, other.provider_id);
        assert!(a.web_url.is_none());
    }
    #[test]
    fn web_login_rejects_plaintext_remote_and_ambiguous_urls() {
        for bad in [
            "http://cloud.example",
            "https://user:pass@cloud.example",
            "https://cloud.example/?x=1",
            "file:///tmp/login",
        ] {
            assert!(
                MemoriaProvider::new(&settings("http://memoria:8100", Some(bad))).is_err(),
                "{bad}"
            );
        }
        for good in [
            "https://thememoria.ai",
            "http://localhost",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(
                MemoriaProvider::new(&settings("http://memoria:8100", Some(good))).is_ok(),
                "{good}"
            );
        }
    }
    #[test]
    fn scoped_access_is_typed_and_fail_closed() {
        assert!(!MemoryAccess::None.allows(false));
        assert!(!MemoryAccess::None.allows(true));
        assert!(MemoryAccess::ReadOnly.allows(false));
        assert!(!MemoryAccess::ReadOnly.allows(true));
        assert!(MemoryAccess::ReadWrite.allows(true));
        for scopes in [
            vec![],
            vec!["identity:read", "keys:manage"],
            vec!["identity:read", "memory:write"],
        ] {
            assert!(
                memory_access_for_scopes(
                    &scopes.into_iter().map(str::to_string).collect::<Vec<_>>()
                )
                .is_none()
            );
        }
    }
}
