//! Canonical identity mapping for already verified provider subjects.
use super::{AuthHttpError, DatabaseAuthService, sha256_hex};
use astra_core::{error_response, internal_error};
use axum::http::StatusCode;
use sqlx::Row;

impl DatabaseAuthService {
    /// Canonical provider-scoped mapping, also used by provider-session login.
    pub(super) async fn resolve_verified_provider_identity(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        provider: &str,
        subject: &str,
        legacy_user: Option<&str>,
    ) -> Result<super::DatabaseUserRecord, AuthHttpError> {
        let existing: Option<String> = sqlx::query_scalar("SELECT astra_user_id FROM auth_external_identities WHERE provider_id = ? AND external_subject = ?")
            .bind(provider).bind(subject).fetch_optional(&mut **tx).await.map_err(internal_error)?;
        let new_account = existing.is_none() && legacy_user.is_none();
        let user = existing.unwrap_or_else(|| {
            legacy_user
                .map(str::to_string)
                .unwrap_or_else(|| format!("ext_{}", sha256_hex(&format!("{provider}\0{subject}"))))
        });
        let username = format!("ext_{}", &sha256_hex(&user)[..24]);
        if new_account {
            sqlx::query("INSERT IGNORE INTO auth_users (user_id,username,email,password_hash,display_name,is_active) VALUES (?, ?, ?, '', 'Astra user', 1)")
                .bind(&user).bind(&username).bind(format!("{username}@external.astra.invalid"))
                .execute(&mut **tx).await.map_err(internal_error)?;
        }
        let row = sqlx::query("SELECT user_id,username,email,password_hash,display_name,is_active FROM auth_users WHERE user_id = ? FOR UPDATE")
            .bind(&user).fetch_optional(&mut **tx).await.map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::FORBIDDEN, "Linked account no longer exists"))?;
        if row.try_get::<i16, _>("is_active").unwrap_or(0) == 0 {
            return Err(error_response(StatusCode::FORBIDDEN, "User is inactive"));
        }
        sqlx::query("INSERT IGNORE INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?, ?, ?)")
            .bind(provider).bind(subject).bind(&user).execute(&mut **tx).await.map_err(internal_error)?;
        if new_account {
            sqlx::query("INSERT IGNORE INTO auth_user_roles (user_id,role_id) SELECT ?, role_id FROM auth_roles WHERE role_name = 'astra_user'")
                .bind(&user).execute(&mut **tx).await.map_err(internal_error)?;
        }
        Ok(super::DatabaseUserRecord {
            user_id: user,
            username: row.try_get("username").map_err(internal_error)?,
            email: row.try_get("email").map_err(internal_error)?,
            password_hash: String::new(),
            display_name: row.try_get("display_name").map_err(internal_error)?,
            is_active: true,
        })
    }
}
