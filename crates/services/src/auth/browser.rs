//! Durable delivery of the current canonical credential pair to Web replicas.
//! The refresh row remains the sole session/revocation authority.
use super::{refresh::session_denied, *};

pub(super) const HANDLE_PREFIX: &str = "bs2.";
const MAX_HANDLE_BYTES: usize = 1600;
const ABSOLUTE_TTL_SECONDS: i64 = 365 * 24 * 60 * 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSessionRequest {
    pub handle: String,
    pub refresh_token: String,
    pub access_token: String,
}

#[derive(Serialize)]
pub struct BrowserSessionCreated {
    pub expires_in: u32,
}

struct Handle {
    hash: String,
    session_id: String,
}

impl Handle {
    fn parse(value: &str) -> Result<Self, AuthHttpError> {
        if value.len() > MAX_HANDLE_BYTES {
            return Err(session_denied());
        }
        let mut parts = value
            .strip_prefix(HANDLE_PREFIX)
            .ok_or_else(session_denied)?
            .split('.');
        let encoded_root = parts.next().ok_or_else(session_denied)?;
        let encoded_session = parts.next().ok_or_else(session_denied)?;
        let secret = parts.next().ok_or_else(session_denied)?;
        if parts.next().is_some()
            || URL_SAFE_NO_PAD
                .decode(secret)
                .ok()
                .filter(|s| s.len() == 32)
                .is_none()
        {
            return Err(session_denied());
        }
        let session = URL_SAFE_NO_PAD
            .decode(encoded_session)
            .map_err(|_| session_denied())?;
        if session.is_empty()
            || session.len() > 64
            || URL_SAFE_NO_PAD.encode(&session) != encoded_session
        {
            return Err(session_denied());
        }
        let session_id = String::from_utf8(session).map_err(|_| session_denied())?;
        let root = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(encoded_root)
                .map_err(|_| session_denied())?,
        )
        .map_err(|_| session_denied())?;
        let url = reqwest::Url::parse(&root).map_err(|_| session_denied())?;
        if root.len() > 1024
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || url.as_str() != root
        {
            return Err(session_denied());
        }
        Ok(Self {
            hash: sha256_hex(value),
            session_id,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Credentials {
    version: u8,
    handle_hash: String,
    token_id: String,
    user_id: String,
    session_id: String,
    access_token: String,
    refresh_token: String,
    absolute_expiry: i64,
}

// Revocation grants no access and needs only the canonical immutable identity.
struct Revocation {
    token_id: String,
    user_id: String,
    session_id: String,
}

struct Owner {
    ciphertext: String,
    credentials: Credentials,
    access_expiry: i64,
    access_issued: i64,
}

impl Owner {
    fn usable(&self) -> bool {
        let lifetime = self.access_expiry.saturating_sub(self.access_issued);
        let margin = 30.min((lifetime / 4).max(0));
        self.access_expiry > Utc::now().timestamp() + margin
    }
    fn response(self, handle: &str) -> AuthTokenRecord {
        AuthTokenRecord {
            user_id: self.credentials.user_id,
            access_token: self.credentials.access_token,
            refresh_token: handle.to_owned(),
            token_type: "bearer".into(),
            expires_in: self
                .access_expiry
                .saturating_sub(Utc::now().timestamp())
                .max(0) as u32,
        }
    }
}

fn unavailable() -> AuthHttpError {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Browser session unavailable; retry or sign in again",
    )
}

impl DatabaseAuthService {
    fn browser_encryptor(&self) -> Result<&FernetTokenEncryptor, AuthHttpError> {
        self.encryptor.as_ref().ok_or_else(unavailable)
    }

    async fn browser_revocation(
        &self,
        pool: &sqlx::MySqlPool,
        handle: &Handle,
    ) -> Result<Option<Revocation>, AuthHttpError> {
        let token: Option<String> =
            sqlx::query_scalar("SELECT token_id FROM auth_browser_sessions WHERE handle_hash = ?")
                .bind(&handle.hash)
                .fetch_optional(pool)
                .await
                .map_err(|_| unavailable())?;
        let Some(token_id) = token else {
            return Ok(None);
        };
        let identity: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT user_id, session_id FROM auth_refresh_tokens WHERE token_id = ?",
        )
        .bind(&token_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| unavailable())?;
        Ok(identity.and_then(|(user_id, session_id)| {
            session_id.map(|session_id| Revocation {
                token_id,
                user_id,
                session_id,
            })
        }))
    }

    /// Point-read the encrypted owner, then validate all canonical authority in
    /// one snapshot with explicit keys. MatrixOne does not reliably propagate a
    /// browser-key filter across an unconstrained multi-table join.
    async fn browser_owner(
        &self,
        pool: &sqlx::MySqlPool,
        handle: &Handle,
    ) -> Result<Option<Owner>, AuthHttpError> {
        let encryptor = self.browser_encryptor()?;
        let mut stale_ciphertext = None;
        for _ in 0..3 {
            let row = query(
                "SELECT encrypted_value, token_id FROM auth_browser_sessions WHERE handle_hash = ?",
            )
            .bind(&handle.hash)
            .fetch_optional(pool)
            .await
            .map_err(|_| unavailable())?;
            let Some(row) = row else {
                return Ok(None);
            };
            let ciphertext: String = row.try_get("encrypted_value").map_err(|_| unavailable())?;
            if stale_ciphertext.as_ref() == Some(&ciphertext) {
                return Err(session_denied());
            }
            if ciphertext.len() > 32768 {
                return Err(unavailable());
            }
            let credentials: Credentials =
                serde_json::from_str(&encryptor.decrypt(&ciphertext).map_err(|_| unavailable())?)
                    .map_err(|_| unavailable())?;
            let refresh = decode_jwt_claims(&credentials.refresh_token, &self.jwt)?;
            let access = jwt::decode_stored_access_claims(&credentials.access_token, &self.jwt)?;
            let (session, origin, _) = self.parse_token_session(&refresh)?;
            let token_id: String = row.try_get("token_id").map_err(|_| unavailable())?;
            if credentials.version != 1
                || credentials.handle_hash != handle.hash
                || credentials.token_id != token_id
                || credentials.session_id != handle.session_id
                || credentials.session_id != session
                || refresh.token_type.as_deref() != Some("refresh")
                || refresh.sub.as_deref() != Some(&credentials.user_id)
                || refresh.exp <= Utc::now().timestamp()
                || credentials.absolute_expiry <= Utc::now().timestamp()
                || access.token_type.as_deref() != Some("access")
                || access.sub != refresh.sub
                || access.sid != refresh.sid
                || access.origin != refresh.origin
            {
                return Err(session_denied());
            }
            // Each table has its own constant key predicate; keep token/account
            // checks in the SAME statement so logout cannot yield a mixed view.
            let canonical = query(
                "SELECT t.token_hash, t.expires_at, m.memoria_bound
                 FROM auth_refresh_tokens t
                 JOIN auth_users u ON u.user_id = ? AND u.user_id = t.user_id
                 LEFT JOIN (SELECT 1 AS memoria_bound FROM auth_external_identities FORCE INDEX (idx_external_identity_user)
                            WHERE astra_user_id = ? AND provider_id LIKE 'memoria:%' LIMIT 1) m ON TRUE
                 WHERE t.token_id = ? AND t.user_id = ? AND t.session_id = ?
                   AND t.is_revoked = 0 AND t.expires_at > NOW(6) AND u.is_active = 1",
            ).bind(&credentials.user_id).bind(&credentials.user_id).bind(&token_id)
                .bind(&credentials.user_id).bind(&session)
                .fetch_optional(pool).await.map_err(|_| unavailable())?;
            let Some(canonical) = canonical else {
                return Ok(None);
            };
            let token_hash: String = canonical.try_get("token_hash").map_err(|_| unavailable())?;
            if sha256_hex(&credentials.refresh_token) != token_hash {
                stale_ciphertext = Some(ciphertext);
                continue;
            }
            let now = Utc::now();
            let canonical_expiry: chrono::NaiveDateTime =
                canonical.try_get("expires_at").map_err(|_| unavailable())?;
            if canonical_expiry <= now.naive_utc()
                || credentials.absolute_expiry <= now.timestamp()
                || refresh.exp <= now.timestamp()
            {
                return Err(session_denied());
            }
            let issued = access.iat.ok_or_else(session_denied)?;
            let memoria_bound: Option<i64> = canonical
                .try_get("memoria_bound")
                .map_err(|_| unavailable())?;
            let access_expiry = if memoria_bound.is_some()
                || origin == "memoria"
                || origin.starts_with("verified:memoria:")
            {
                access
                    .exp
                    .min(issued.saturating_add(i64::from(memoria::ACCESS_TTL_SECONDS)))
            } else {
                access.exp
            };
            return Ok(Some(Owner {
                ciphertext,
                access_expiry: access_expiry
                    .min(canonical_expiry.and_utc().timestamp())
                    .min(credentials.absolute_expiry),
                credentials,
                access_issued: issued,
            }));
        }
        Err(unavailable())
    }

    fn encrypted_successor(
        &self,
        handle: &Handle,
        rotation: &mut refresh::PreparedRefresh,
        absolute_expiry: i64,
    ) -> Result<String, AuthHttpError> {
        if absolute_expiry <= Utc::now().timestamp() {
            return Err(session_denied());
        }
        let credentials = Credentials {
            version: 1,
            handle_hash: handle.hash.clone(),
            token_id: rotation.token_id.clone(),
            user_id: rotation.user_id.clone(),
            session_id: rotation.session_id.clone(),
            access_token: rotation.tokens.access_token.clone(),
            refresh_token: rotation.tokens.refresh_token.clone(),
            absolute_expiry,
        };
        let value = serde_json::to_string(&credentials).map_err(|_| unavailable())?;
        rotation.expires_at = chrono::DateTime::from_timestamp(absolute_expiry, 0)
            .ok_or_else(session_denied)?
            .naive_utc()
            .min(rotation.expires_at);
        self.browser_encryptor()?
            .encrypt(&value)
            .map_err(|_| unavailable())
    }

    pub(super) async fn adopt_browser_session(
        &self,
        request: BrowserSessionRequest,
    ) -> Result<BrowserSessionCreated, AuthHttpError> {
        let handle = Handle::parse(&request.handle)?;
        self.browser_encryptor()?;
        let pool = self.get_pool().await.map_err(|_| unavailable())?;
        // Also recovers an acknowledgement lost after committing creation. The
        // independent handle, never a consumed refresh JWT, proves ownership.
        if let Some(owner) = self.browser_owner(&pool, &handle).await? {
            return Ok(BrowserSessionCreated {
                expires_in: owner
                    .credentials
                    .absolute_expiry
                    .saturating_sub(Utc::now().timestamp())
                    .max(0) as u32,
            });
        }
        for _ in 0..3 {
            let result = self.create_browser_owner(&pool, &handle, &request).await;
            if result.is_err() && self.browser_owner(&pool, &handle).await?.is_some() {
                return Ok(BrowserSessionCreated {
                    expires_in: ABSOLUTE_TTL_SECONDS as u32,
                });
            }
            match result {
                Ok(created) => return Ok(created),
                Err(refresh::CommitRefreshError::BindingChanged) => continue,
                Err(refresh::CommitRefreshError::Auth(error)) => return Err(error),
            }
        }
        Err(unavailable())
    }

    async fn create_browser_owner(
        &self,
        pool: &sqlx::MySqlPool,
        handle: &Handle,
        request: &BrowserSessionRequest,
    ) -> Result<BrowserSessionCreated, refresh::CommitRefreshError> {
        if request.access_token.len() > 8192 {
            return Err(session_denied().into());
        }
        let mut rotation = self.prepare_refresh(pool, &request.refresh_token).await?;
        if rotation.session_id != handle.session_id {
            return Err(session_denied().into());
        }
        let access = jwt::decode_stored_access_claims(&request.access_token, &self.jwt)?;
        let refresh = decode_jwt_claims(&request.refresh_token, &self.jwt)?;
        if access.token_type.as_deref() != Some("access")
            || access.sub != refresh.sub
            || access.sid != refresh.sid
            || access.origin != refresh.origin
        {
            return Err(session_denied().into());
        }
        let ciphertext = self.encrypted_successor(
            handle,
            &mut rotation,
            Utc::now().timestamp() + ABSOLUTE_TTL_SECONDS,
        )?;
        let mut tx = pool.begin().await.map_err(|_| unavailable())?;
        self.lock_refresh_account(&mut tx, &rotation.user_id)
            .await?;
        if let Err(error) = self.commit_refresh(&mut tx, &rotation).await {
            if matches!(error, refresh::CommitRefreshError::BindingChanged) {
                tx.rollback().await.map_err(|_| unavailable())?;
            }
            return Err(error);
        }
        query("INSERT INTO auth_browser_sessions (handle_hash, token_id, encrypted_value) VALUES (?, ?, ?)")
            .bind(&handle.hash).bind(&rotation.token_id).bind(ciphertext)
            .execute(&mut *tx).await.map_err(|_| unavailable())?;
        tx.commit().await.map_err(|_| unavailable())?;
        Ok(BrowserSessionCreated {
            expires_in: ABSOLUTE_TTL_SECONDS as u32,
        })
    }

    pub(super) async fn resolve_browser_session(
        &self,
        value: &str,
    ) -> Result<AuthTokenRecord, AuthHttpError> {
        let handle = Handle::parse(value)?;
        let pool = self.get_pool().await.map_err(|_| unavailable())?;
        let mut owner = self
            .browser_owner(&pool, &handle)
            .await?
            .ok_or_else(session_denied)?;
        for _ in 0..3 {
            if owner.usable() {
                return Ok(owner.response(value));
            }
            let result = self.rotate_browser_owner(&pool, &handle, &owner).await;
            // Always use a fresh snapshot after releasing the transaction,
            // including a conflict or unknown commit outcome. Never return an
            // uncommitted in-memory successor.
            let current = self
                .browser_owner(&pool, &handle)
                .await?
                .ok_or_else(session_denied)?;
            if current.ciphertext == owner.ciphertext {
                match result {
                    Err(refresh::CommitRefreshError::BindingChanged) => {}
                    Err(refresh::CommitRefreshError::Auth(error)) => return Err(error),
                    Ok(()) => {}
                }
            }
            owner = current;
        }
        if owner.usable() {
            Ok(owner.response(value))
        } else {
            Err(unavailable())
        }
    }

    async fn rotate_browser_owner(
        &self,
        pool: &sqlx::MySqlPool,
        handle: &Handle,
        owner: &Owner,
    ) -> Result<(), refresh::CommitRefreshError> {
        let mut rotation = self
            .prepare_refresh(pool, &owner.credentials.refresh_token)
            .await?;
        let ciphertext =
            self.encrypted_successor(handle, &mut rotation, owner.credentials.absolute_expiry)?;
        let mut tx = pool.begin().await.map_err(|_| unavailable())?;
        self.lock_refresh_account(&mut tx, &rotation.user_id)
            .await?;
        if owner.credentials.absolute_expiry <= Utc::now().timestamp() {
            return Err(session_denied().into());
        }
        if let Err(error) = self.commit_refresh(&mut tx, &rotation).await {
            if matches!(error, refresh::CommitRefreshError::BindingChanged) {
                tx.rollback().await.map_err(|_| unavailable())?;
            }
            return Err(error);
        }
        // Exact canonical hash CAS fences all legitimate ciphertext changes;
        // immutable handle/token identity suffices, with no TEXT collation CAS.
        let updated = query("UPDATE auth_browser_sessions SET encrypted_value = ? WHERE handle_hash = ? AND token_id = ?")
            .bind(ciphertext).bind(&handle.hash).bind(&rotation.token_id).execute(&mut *tx).await.map_err(|_| unavailable())?;
        if updated.rows_affected() != 1 {
            return Err(session_denied().into());
        }
        tx.commit().await.map_err(|_| unavailable())?;
        Ok(())
    }

    async fn revoke_browser_owner(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        handle: &Handle,
        credentials: &Revocation,
    ) -> Result<(), AuthHttpError> {
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_id = ? AND user_id = ? AND session_id = ?")
            .bind(&credentials.token_id).bind(&credentials.user_id).bind(&credentials.session_id).execute(&mut **tx).await.map_err(|_| unavailable())?;
        query("DELETE FROM auth_browser_sessions WHERE handle_hash = ? AND token_id = ?")
            .bind(&handle.hash)
            .bind(&credentials.token_id)
            .execute(&mut **tx)
            .await
            .map_err(|_| unavailable())?;
        Ok(())
    }

    pub(super) async fn logout_browser_session(&self, value: &str) -> Result<(), AuthHttpError> {
        let handle = Handle::parse(value)?;
        let pool = self.get_pool().await.map_err(|_| unavailable())?;
        let Some(owner) = self.browser_revocation(&pool, &handle).await? else {
            return Ok(());
        };
        let _invalidation = self
            .authentication_cache
            .session_mutation(&owner.user_id, &owner.session_id);
        let mut tx = pool.begin().await.map_err(|_| unavailable())?;
        self.lock_session_account(&mut tx, &owner.user_id).await?;
        self.revoke_browser_owner(&mut tx, &handle, &owner).await?;
        tx.commit().await.map_err(|_| unavailable())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_require_canonical_runtime_and_256_bit_proof() {
        for root in ["https://astra.example/", "http://localhost:17001/"] {
            let value = format!(
                "bs2.{}.{}.{}",
                URL_SAFE_NO_PAD.encode(root),
                URL_SAFE_NO_PAD.encode("session-id"),
                URL_SAFE_NO_PAD.encode([7; 32])
            );
            assert_eq!(Handle::parse(&value).unwrap().hash, sha256_hex(&value));
        }
        for root in [
            "https://astra.example",
            "https://user:secret@astra.example/",
            "https://astra.example/path",
            "file:///",
            "https://astra.example/?x=1",
        ] {
            let value = format!(
                "bs2.{}.{}.{}",
                URL_SAFE_NO_PAD.encode(root),
                URL_SAFE_NO_PAD.encode("session-id"),
                URL_SAFE_NO_PAD.encode([7; 32])
            );
            assert!(Handle::parse(&value).is_err());
        }
        for value in ["", "bs1.x.y", "bs2.a.b.c.d", "bs2.!!!!.!.!"] {
            assert!(Handle::parse(value).is_err());
        }
        assert!(Handle::parse(&"x".repeat(MAX_HANDLE_BYTES + 1)).is_err());
    }
}
