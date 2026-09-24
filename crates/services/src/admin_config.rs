//! Server-wide admin configuration KV store.
//!
//! Persists admin-controlled settings such as `reasoning_offering_id` in the `admin_config`
//! table. Only keys in [`ADMIN_CONFIG_ALLOWED_KEYS`] may be stored.

use async_trait::async_trait;
use sqlx::{Row, query};

use astra_core::{MatrixOneSettings, SharedPool};

/// Admin-configurable key: the preferred reasoning/judge/summary Offering.
///
/// The value is an exact active `infra_llm_models.model_id` during the Phase 0
/// catalog. Model names and aliases are display facts and cannot select a
/// route.
pub const ADMIN_CONFIG_KEY_REASONING_OFFERING: &str = "reasoning_offering_id";

/// Whitelist of admin config keys the server will accept.
pub const ADMIN_CONFIG_KEY_JUDGMENT_OFFERING: &str = "judgment_offering_id";
pub const ADMIN_CONFIG_ALLOWED_KEYS: &[&str] = &[
    ADMIN_CONFIG_KEY_REASONING_OFFERING,
    ADMIN_CONFIG_KEY_JUDGMENT_OFFERING,
];

/// Resolve the optional judgment route under the requesting user's policy.
/// An invalid configured route is an error, never a silent switch to another model.
pub async fn resolve_judgment_offering(
    config: &dyn AdminConfigService,
    models: &dyn crate::models::ModelService,
    user_id: &str,
) -> Result<
    Option<crate::models::AdmittedModelExecution>,
    (
        axum::http::StatusCode,
        axum::Json<astra_core::ErrorResponse>,
    ),
> {
    let Some(id) = config
        .get(ADMIN_CONFIG_KEY_JUDGMENT_OFFERING)
        .await
        .map_err(astra_core::internal_error)?
    else {
        return Ok(None);
    };
    let admitted = models.admit_model_offering(user_id.to_string(), id).await?;
    if admitted.provider == "typesafe" && admitted.api_key.trim().is_empty() {
        return Err(astra_core::error_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Configured TypeSafe judgment Offering has no API key",
        ));
    }
    Ok(Some(admitted))
}

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

fn validate_value(key: &str, value: &str) -> Result<(), String> {
    validate_key(key)?;
    match key {
        ADMIN_CONFIG_KEY_REASONING_OFFERING | ADMIN_CONFIG_KEY_JUDGMENT_OFFERING => {
            crate::models::validate_model_offering_id(value)
                .map(|_| ())
                .map_err(|_| format!("{key} must be an exact Offering ID"))
        }
        _ => Err(format!("admin config key '{key}' has no value contract")),
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
        crate::require_shared_pool_message(
            self.pool.as_ref(),
            "DatabaseAdminConfigService",
            &self.matrixone,
        )
    }
}

#[async_trait]
impl AdminConfigService for DatabaseAdminConfigService {
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        validate_key(key)?;
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
        validate_value(key, value)?;
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
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        validate_key(key)?;
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
    fn allowed_keys_includes_reasoning_offering() {
        assert!(ADMIN_CONFIG_ALLOWED_KEYS.contains(&ADMIN_CONFIG_KEY_REASONING_OFFERING));
    }

    #[test]
    fn validate_key_accepts_allowed() {
        assert!(validate_key(ADMIN_CONFIG_KEY_REASONING_OFFERING).is_ok());
    }

    #[test]
    fn validate_key_rejects_unknown() {
        let err = validate_key("arbitrary_key").unwrap_err();
        assert!(err.contains("unknown admin config key"));
        assert!(err.contains("arbitrary_key"));
        assert!(err.contains(ADMIN_CONFIG_KEY_REASONING_OFFERING));
    }

    #[test]
    fn reasoning_offering_value_rejects_empty_or_normalized_identity() {
        assert!(validate_value(ADMIN_CONFIG_KEY_REASONING_OFFERING, "").is_err());
        assert!(validate_value(ADMIN_CONFIG_KEY_REASONING_OFFERING, " offer-1").is_err());
        assert!(validate_value(ADMIN_CONFIG_KEY_REASONING_OFFERING, "offer-1").is_ok());
    }

    // get() must reject unknown keys — callers should not silently read
    // a non-existent key and get None back as if it were "just unset".
    #[tokio::test]
    async fn get_unknown_key_returns_err() {
        let svc = UnconfiguredAdminConfigService;
        let result = svc.get("not_a_real_key").await;
        assert!(
            result.is_err(),
            "get() with unknown key must return Err, not Ok(None)"
        );
        let err = result.unwrap_err();
        assert!(err.contains("unknown admin config key"), "err: {err}");
    }

    #[tokio::test]
    async fn get_known_key_does_not_err_on_unconfigured() {
        let svc = UnconfiguredAdminConfigService;
        // Known key on an unconfigured service returns Ok(None), not Err.
        let result = svc.get(ADMIN_CONFIG_KEY_REASONING_OFFERING).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    use crate::models::{
        AdmittedModelExecution, ModelAccessKind, ModelCreateRequestData, ModelExecutionPlacement,
        ModelListItem, ModelRecord, ModelService, ModelUpdateRequestData, ResolvedModelOffering,
        UnconfiguredModelService,
    };
    use axum::{Json, http::StatusCode};
    use std::sync::Mutex;

    struct FixedConfig(Result<Option<String>, String>);

    #[async_trait]
    impl AdminConfigService for FixedConfig {
        async fn get(&self, key: &str) -> Result<Option<String>, String> {
            assert_eq!(key, ADMIN_CONFIG_KEY_JUDGMENT_OFFERING);
            self.0.clone()
        }
        async fn list(&self) -> Result<Vec<(String, String)>, String> {
            unreachable!()
        }
        async fn set(&self, _: &str, _: &str, _: Option<&str>) -> Result<(), String> {
            unreachable!()
        }
        async fn unset(&self, _: &str) -> Result<bool, String> {
            unreachable!()
        }
    }

    type ModelResult<T> = Result<T, (StatusCode, Json<astra_core::ErrorResponse>)>;

    struct AdmissionMock {
        admitted: AdmittedModelExecution,
        denied: bool,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl AdmissionMock {
        fn new(provider: &str, api_key: &str) -> Self {
            Self {
                admitted: AdmittedModelExecution {
                    offering_id: "judge-offering".into(),
                    access_kind: ModelAccessKind::SelfHosted,
                    execution_placement: ModelExecutionPlacement::Server,
                    model_name: "judge-model".into(),
                    wire_model_name: None,
                    api_key: api_key.into(),
                    base_url: "https://judgment.example.invalid".into(),
                    provider: provider.into(),
                    pricing: None,
                    cache_capability: None,
                    thinking_capability: None,
                    fixed_temperature: None,
                    thinking_protocol: None,
                    request_body_overrides: None,
                    context_window: None,
                    max_completion_tokens: None,
                    header_overrides: Default::default(),
                    completions_url_override: None,
                    request_timeout_ms: None,
                },
                denied: false,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelService for AdmissionMock {
        async fn admit_model_offering(
            &self,
            user: String,
            offering: String,
        ) -> ModelResult<AdmittedModelExecution> {
            self.calls.lock().unwrap().push((user, offering));
            if self.denied {
                Err(astra_core::error_response(
                    StatusCode::FORBIDDEN,
                    "Offering permission denied",
                ))
            } else {
                Ok(self.admitted.clone())
            }
        }
        async fn create_model(
            &self,
            user: String,
            request: ModelCreateRequestData,
        ) -> ModelResult<ModelRecord> {
            UnconfiguredModelService.create_model(user, request).await
        }
        async fn list_models(&self, user: String, admin: bool) -> ModelResult<Vec<ModelListItem>> {
            UnconfiguredModelService.list_models(user, admin).await
        }
        async fn get_model(&self, name: String) -> ModelResult<ModelRecord> {
            UnconfiguredModelService.get_model(name).await
        }
        async fn resolve_model_offering(&self, id: String) -> ModelResult<ResolvedModelOffering> {
            UnconfiguredModelService.resolve_model_offering(id).await
        }
        async fn update_model(
            &self,
            name: String,
            request: ModelUpdateRequestData,
        ) -> ModelResult<ModelRecord> {
            UnconfiguredModelService.update_model(name, request).await
        }
        async fn delete_model(&self, name: String) -> ModelResult<()> {
            UnconfiguredModelService.delete_model(name).await
        }
        async fn check_model(&self, name: String) -> ModelResult<ModelRecord> {
            UnconfiguredModelService.check_model(name).await
        }
    }

    #[tokio::test]
    async fn judgment_route_preserves_permission_denial() {
        let config = FixedConfig(Ok(Some("judge-offering".into())));
        let mut models = AdmissionMock::new("typesafe", "test-key");
        models.denied = true;
        let (status, body) = resolve_judgment_offering(&config, &models, "user-a")
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.detail, "Offering permission denied");
        assert_eq!(
            *models.calls.lock().unwrap(),
            vec![("user-a".into(), "judge-offering".into())]
        );
    }

    #[tokio::test]
    async fn judgment_route_rejects_typesafe_without_credentials() {
        let config = FixedConfig(Ok(Some("judge-offering".into())));
        let models = AdmissionMock::new("typesafe", " ");
        let (status, body) = resolve_judgment_offering(&config, &models, "user-a")
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.detail.contains("no API key"));
        assert_eq!(models.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn judgment_route_config_read_failure_is_an_error() {
        let config = FixedConfig(Err("configuration unavailable".into()));
        let models = AdmissionMock::new("typesafe", "test-key");
        let (status, _) = resolve_judgment_offering(&config, &models, "user-a")
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(models.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn judgment_route_unset_config_returns_none_without_admission() {
        let models = AdmissionMock::new("typesafe", "test-key");
        let result = resolve_judgment_offering(&UnconfiguredAdminConfigService, &models, "user-a")
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(models.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn judgment_route_admits_exact_offering_for_requesting_user() {
        let config = FixedConfig(Ok(Some("judge-offering".into())));
        for provider in ["typesafe", "openai"] {
            let models = AdmissionMock::new(provider, "test-key");
            let result = resolve_judgment_offering(&config, &models, "user-b")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, models.admitted);
            assert_eq!(
                *models.calls.lock().unwrap(),
                vec![("user-b".into(), "judge-offering".into())]
            );
        }
    }
}
