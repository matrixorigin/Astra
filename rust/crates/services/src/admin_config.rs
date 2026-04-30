//! Server-wide admin configuration KV store.
//!
//! Persists admin-controlled settings such as `reasoning_model_name` in the `admin_config`
//! table. Only keys in [`ADMIN_CONFIG_ALLOWED_KEYS`] may be stored.

use async_trait::async_trait;
use sqlx::{Row, query};

use astra_core::{MatrixOneSettings, SharedPool, connect_matrixone};

/// Admin-configurable key: the preferred reasoning/judge/summary model name. Must reference
/// an active row in `infra_llm_models`.
pub const ADMIN_CONFIG_KEY_REASONING_MODEL: &str = "reasoning_model_name";

/// Whitelist of admin config keys the server will accept.
pub const ADMIN_CONFIG_ALLOWED_KEYS: &[&str] = &[ADMIN_CONFIG_KEY_REASONING_MODEL];

#[async_trait]
pub trait AdminConfigService: Send + Sync {
    /// Get the value for `key`, or `None` if unset.
    async fn get(&self, key: &str) -> Result<Option<String>, String>;

    /// List all stored (key, value) pairs, sorted by key.
    async fn list(&self) -> Result<Vec<(String, String)>, String>;

    /// Upsert `key` → `value`. Rejects keys not in [`ADMIN_CONFIG_ALLOWED_KEYS`].
    async fn set(&self, key: &str, value: &str, updated_by: Option<&str>) -> Result<(), String>;

    /// Delete `key`. Returns `true` if a row was removed.
    async fn unset(&self, key: &str) -> Result<bool, String>;
}

fn validate_key(key: &str) -> Result<(), String> {
    if ADMIN_CONFIG_ALLOWED_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!(
            "unknown admin config key '{key}'. Allowed keys: {}",
            ADMIN_CONFIG_ALLOWED_KEYS.join(", ")
        ))
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminConfigService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminConfigService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, String> {
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        connect_matrixone(&self.matrixone)
            .await
            .map_err(|e| format!("DB connect: {e}"))
    }
}

#[async_trait]
impl AdminConfigService for DatabaseAdminConfigService {
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        let pool = self.get_pool().await?;
        let row = query("SELECT config_value FROM admin_config WHERE config_key = ?")
            .bind(key)
            .fetch_optional(&pool)
            .await
            .map_err(|e| format!("DB query: {e}"))?;
        match row {
            Some(r) => {
                let v: String = r.try_get("config_value").map_err(|e| e.to_string())?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    async fn list(&self) -> Result<Vec<(String, String)>, String> {
        let pool = self.get_pool().await?;
        let rows =
            query("SELECT config_key, config_value FROM admin_config ORDER BY config_key ASC")
                .fetch_all(&pool)
                .await
                .map_err(|e| format!("DB query: {e}"))?;
        rows.iter()
            .map(|r| {
                let k: String = r.try_get("config_key").map_err(|e| e.to_string())?;
                let v: String = r.try_get("config_value").map_err(|e| e.to_string())?;
                Ok((k, v))
            })
            .collect()
    }

    async fn set(&self, key: &str, value: &str, updated_by: Option<&str>) -> Result<(), String> {
        validate_key(key)?;
        let pool = self.get_pool().await?;
        query(
            "INSERT INTO admin_config (config_key, config_value, updated_by, updated_at) \
             VALUES (?, ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE config_value = VALUES(config_value), \
                                     updated_by = VALUES(updated_by), \
                                     updated_at = NOW()",
        )
        .bind(key)
        .bind(value)
        .bind(updated_by)
        .execute(&pool)
        .await
        .map_err(|e| format!("DB upsert: {e}"))?;
        Ok(())
    }

    async fn unset(&self, key: &str) -> Result<bool, String> {
        let pool = self.get_pool().await?;
        let result = query("DELETE FROM admin_config WHERE config_key = ?")
            .bind(key)
            .execute(&pool)
            .await
            .map_err(|e| format!("DB delete: {e}"))?;
        Ok(result.rows_affected() > 0)
    }
}

/// Stub that rejects every operation — used when the server is running in a mode where admin
/// config is unavailable (e.g. tests, partial wiring).
#[derive(Clone, Debug, Default)]
pub struct UnconfiguredAdminConfigService;

#[async_trait]
impl AdminConfigService for UnconfiguredAdminConfigService {
    async fn get(&self, _key: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn list(&self) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }

    async fn set(&self, _key: &str, _value: &str, _updated_by: Option<&str>) -> Result<(), String> {
        Err("admin config service is not configured on this server".into())
    }

    async fn unset(&self, _key: &str) -> Result<bool, String> {
        Err("admin config service is not configured on this server".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_keys_includes_reasoning_model() {
        assert!(ADMIN_CONFIG_ALLOWED_KEYS.contains(&ADMIN_CONFIG_KEY_REASONING_MODEL));
    }

    #[test]
    fn validate_key_accepts_allowed() {
        assert!(validate_key(ADMIN_CONFIG_KEY_REASONING_MODEL).is_ok());
    }

    #[test]
    fn validate_key_rejects_unknown() {
        let err = validate_key("arbitrary_key").unwrap_err();
        assert!(err.contains("unknown admin config key"));
        assert!(err.contains("arbitrary_key"));
        assert!(err.contains(ADMIN_CONFIG_KEY_REASONING_MODEL));
    }
}
