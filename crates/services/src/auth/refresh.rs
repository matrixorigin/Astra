//! One refresh preparation/commit path for direct JWT and browser-owned sessions.
use super::*;

pub(super) enum CommitRefreshError {
    BindingChanged,
    Auth(AuthHttpError),
}

impl From<AuthHttpError> for CommitRefreshError {
    fn from(error: AuthHttpError) -> Self {
        Self::Auth(error)
    }
}

pub(super) enum BrowserPublication<'a> {
    Adopt {
        handle_hash: &'a str,
        ciphertext: &'a str,
    },
    Rotate {
        handle_hash: &'a str,
        ciphertext: &'a str,
    },
}

pub(super) struct PreparedRefresh {
    pub user_id: String,
    pub session_id: String,
    pub old_hash: String,
    pub old_expiry: i64,
    pub tokens: AuthTokenRecord,
    pub expires_at: chrono::NaiveDateTime,
    memoria_owner: Option<String>,
    memoria_binding: Option<memoria::MemoriaRefreshBinding>,
}

pub(super) fn session_denied() -> AuthHttpError {
    error_response(
        StatusCode::UNAUTHORIZED,
        "Session expired or revoked; sign in again",
    )
}

impl DatabaseAuthService {
    pub(super) async fn prepare_refresh(
        &self,
        pool: &sqlx::MySqlPool,
        refresh_token: &str,
    ) -> Result<PreparedRefresh, AuthHttpError> {
        if refresh_token.len() > 8192 {
            return Err(session_denied());
        }
        let claims =
            decode_jwt_claims_with_detail(refresh_token, &self.jwt, "Invalid refresh token")?;
        if claims.token_type.as_deref() != Some("refresh") || claims.exp <= Utc::now().timestamp() {
            return Err(session_denied());
        }
        let user_id = claims.sub.as_deref().ok_or_else(session_denied)?;
        let (session_id, origin, _) = self.parse_token_session(&claims)?;
        let old_hash = sha256_hex(refresh_token);
        let username: Option<String> = sqlx::query_scalar(
            "SELECT u.username FROM auth_refresh_tokens t
             JOIN auth_users u ON u.user_id = ? AND u.user_id = t.user_id
             WHERE t.session_id = ? AND t.token_hash = ? AND t.user_id = ?
               AND t.is_revoked = 0 AND t.expires_at > NOW(6) AND u.is_active = 1",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(&old_hash)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| map_auth_sqlx(error, "refresh.fetch_token", Some(pool)))?;
        let username = username.ok_or_else(session_denied)?;
        let memoria_owner = self.memoria_owner(pool, user_id).await?;
        let memoria_binding = if let Some(owner) = &memoria_owner {
            Some(
                self.revalidate_memoria_connection(pool, user_id, owner)
                    .await?,
            )
        } else {
            if origin == "memoria" || origin.starts_with("verified:memoria:") {
                return Err(session_denied());
            }
            None
        };
        let origin = if memoria_binding.is_some() {
            format!(
                "verified:{}",
                self.memoria_provider
                    .as_ref()
                    .ok_or_else(session_denied)?
                    .provider_id
            )
        } else {
            origin
        };
        let access_token = self
            .create_access_token(user_id, &username, &session_id, &origin)
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(user_id, &session_id, &origin)
            .map_err(internal_error)?;
        let expiry = decode_jwt_claims(&refresh_token, &self.jwt)?.exp;
        let expires_at = chrono::DateTime::from_timestamp(expiry, 0)
            .ok_or_else(session_denied)?
            .naive_utc();
        let expires_in = decode_jwt_claims(&access_token, &self.jwt)?
            .exp
            .saturating_sub(Utc::now().timestamp())
            .max(0) as u32;
        Ok(PreparedRefresh {
            user_id: user_id.to_owned(),
            session_id,
            old_hash,
            old_expiry: claims.exp,
            tokens: AuthTokenRecord {
                user_id: user_id.to_owned(),
                access_token,
                refresh_token,
                token_type: "bearer".into(),
                expires_in,
            },
            expires_at,
            memoria_owner,
            memoria_binding,
        })
    }

    pub(super) async fn lock_session_account(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        user: &str,
    ) -> Result<Option<bool>, AuthHttpError> {
        let active: Option<i16> =
            sqlx::query_scalar("SELECT is_active FROM auth_users WHERE user_id = ? FOR UPDATE")
                .bind(user)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|error| map_auth_sqlx(error, "refresh.lock_account", None))?;
        Ok(active.map(|value| value == 1))
    }

    pub(super) async fn lock_refresh_account(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        user: &str,
    ) -> Result<(), AuthHttpError> {
        if self.lock_session_account(tx, user).await? != Some(true) {
            return Err(session_denied());
        }
        Ok(())
    }

    /// Caller holds the account lock. No external work is permitted here.
    pub(super) async fn commit_refresh(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        prepared: &PreparedRefresh,
        browser: Option<BrowserPublication<'_>>,
    ) -> Result<(), CommitRefreshError> {
        if prepared.old_expiry <= Utc::now().timestamp()
            || prepared.expires_at <= Utc::now().naive_utc()
        {
            return Err(session_denied().into());
        }
        // Re-read after acquiring the account lock, including a first-ever
        // provider link that raced preparation of an internal session.
        if self.memoria_owner(&mut **tx, &prepared.user_id).await? != prepared.memoria_owner {
            return Err(CommitRefreshError::BindingChanged);
        }
        if let Some(binding) = &prepared.memoria_binding
            && !binding.check(tx, &prepared.user_id).await?
        {
            return Err(CommitRefreshError::BindingChanged);
        }
        let (handle_hash, ciphertext, guard) = match &browser {
            Some(BrowserPublication::Adopt {
                handle_hash,
                ciphertext,
            }) => (
                Some(*handle_hash),
                Some(*ciphertext),
                " AND browser_handle_hash IS NULL AND browser_ciphertext IS NULL",
            ),
            Some(BrowserPublication::Rotate {
                handle_hash,
                ciphertext,
            }) => (
                Some(*handle_hash),
                Some(*ciphertext),
                " AND browser_handle_hash = ?",
            ),
            None => (None, None, ""),
        };
        let statement = format!("UPDATE auth_refresh_tokens SET token_hash = ?, expires_at = ?, created_at = NOW(6), browser_handle_hash = ?, browser_ciphertext = ?
             WHERE session_id = ? AND token_hash = ? AND user_id = ?
               AND is_revoked = 0 AND expires_at > NOW(6){guard}");
        // `guard` is selected exclusively from the fixed literals above.
        let mut update = query(sqlx::AssertSqlSafe(statement.as_str()))
            .bind(sha256_hex(&prepared.tokens.refresh_token))
            .bind(prepared.expires_at)
            .bind(handle_hash)
            .bind(ciphertext)
            .bind(&prepared.session_id)
            .bind(&prepared.old_hash)
            .bind(&prepared.user_id);
        if matches!(browser, Some(BrowserPublication::Rotate { .. })) {
            update = update.bind(handle_hash);
        }
        let updated = update
            .execute(&mut **tx)
            .await
            .map_err(|error| map_auth_sqlx(error, "refresh.database", None))?;
        if updated.rows_affected() != 1 {
            return Err(session_denied().into());
        }
        Ok(())
    }
}
