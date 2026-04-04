use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use crate::auth::FernetTokenEncryptor;
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PricingData {
    #[serde(default)]
    pub prompt: f64,
    #[serde(default)]
    pub completion: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct QuirksData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_temperature: Option<f64>,
    #[serde(default)]
    pub preserve_reasoning_content: bool,
    #[serde(default)]
    pub no_parallel_tool_calls: bool,
    #[serde(default)]
    pub tool_choice_required: bool,
    #[serde(default)]
    pub strict_tool_call_ids: bool,
    #[serde(default)]
    pub no_system_message: bool,
    #[serde(default)]
    pub system_as_user_prefix: bool,
    /// Fallback model name to use when the primary model hits rate limits.
    /// Must reference an active model in `infra_llm_models`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelCreateRequestData {
    pub name: String,
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: Option<QuirksData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUpdateRequestData {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
    pub pricing: Option<PricingData>,
    pub architecture: Option<String>,
    pub tags: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub quirks: Option<QuirksData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRecord {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: QuirksData,
    pub connectivity: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelListItem {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub architecture: Option<String>,
}

/// Decrypted credentials for the active (or preferred) row in `infra_llm_models`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedActiveLlmModel {
    pub model_name: String,
    pub api_key: String,
    pub base_url: String,
    pub provider: String,
    /// Fallback model name from quirks (cloud-managed).
    pub fallback_model: Option<String>,
}

/// Resolve the active LLM model from the database for in-process / server-side callers.
///
/// If `preferred` is `Some(name)`, selects that model when `is_active = 1`; otherwise uses
/// the lexicographically first active model. When `pool` is `None`, opens an ephemeral
/// single-connection pool from `matrixone`.
///
/// Also extracts `fallback_model` from the `quirks` JSON column (cloud-managed config).
pub async fn resolve_active_llm_model(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    preferred: Option<&str>,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
) -> Result<ResolvedActiveLlmModel, String> {
    let ephemeral;
    let pool: &sqlx::Pool<sqlx::MySql> = match pool {
        Some(p) => p,
        None => {
            ephemeral = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect(&matrixone.database_url())
                .await
                .map_err(|e| format!("DB connect: {e}"))?;
            &ephemeral
        }
    };

    let row = if let Some(name) = preferred {
        sqlx::query(
            "SELECT model_name, api_key_encrypted, base_url, provider, \
                    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json \
             FROM infra_llm_models WHERE model_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB query: {e}"))?
    } else {
        None
    };

    let row = if row.is_none() {
        sqlx::query(
            "SELECT model_name, api_key_encrypted, base_url, provider, \
                    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json \
             FROM infra_llm_models WHERE is_active = 1 ORDER BY model_name LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB query fallback: {e}"))?
    } else {
        row
    };

    let row = row
        .ok_or_else(|| "No active LLM model configured. Run: astra-admin model add".to_string())?;

    let model_name: String = row.try_get("model_name").map_err(|e| e.to_string())?;
    let encrypted: String = row
        .try_get("api_key_encrypted")
        .map_err(|e| e.to_string())?;
    let base_url: String = row
        .try_get("base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let provider: String = row
        .try_get("provider")
        .unwrap_or_else(|_| "openai".to_string());
    let api_key = encryptor
        .decrypt(&encrypted)
        .map_err(|e| format!("Decrypt: {e}"))?;

    // Extract fallback_model from quirks JSON (env var override if set)
    let fallback_model = {
        let quirks_json: String = row
            .try_get("quirks_json")
            .unwrap_or_else(|_| "{}".to_string());
        let quirks: QuirksData = serde_json::from_str(&quirks_json).unwrap_or_default();
        // Env var overrides DB config
        std::env::var("MO_LLM_FALLBACK_MODEL")
            .ok()
            .filter(|v| !v.is_empty())
            .or(quirks.fallback_model)
    };

    Ok(ResolvedActiveLlmModel {
        model_name,
        api_key,
        base_url,
        provider,
        fallback_model,
    })
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ModelService: Send + Sync {
    async fn create_model(
        &self,
        user_id: String,
        request: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_models(
        &self,
        user_id: String,
        is_admin: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn update_model(
        &self,
        model_name: String,
        request: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_model(
        &self,
        model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone)]
pub struct DatabaseModelService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    encryptor: std::sync::Arc<FernetTokenEncryptor>,
}

impl DatabaseModelService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: std::sync::Arc<FernetTokenEncryptor>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            pool: None,
        }
    }

    fn model_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);
        let input_mod_json: String = row
            .try_get("input_modalities_json")
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let output_mod_json: String = row
            .try_get("output_modalities_json")
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let supported_json: String = row
            .try_get("supported_parameters_json")
            .unwrap_or_else(|_| "[]".to_string());
        let pricing_json: String = row
            .try_get("pricing_json")
            .unwrap_or_else(|_| "{}".to_string());
        let tags_json: String = row
            .try_get("tags_json")
            .unwrap_or_else(|_| "[]".to_string());
        let quirks_json: String = row
            .try_get("quirks_json")
            .unwrap_or_else(|_| "{}".to_string());

        Ok(ModelRecord {
            model_id: row.try_get("model_id").map_err(internal_error)?,
            name: row.try_get("model_name").map_err(internal_error)?,
            provider: row.try_get("provider").map_err(internal_error)?,
            base_url: row.try_get("base_url").ok(),
            description: row.try_get("description").ok(),
            is_active: is_active_int != 0,
            context_window: row.try_get("context_window").unwrap_or(128000),
            max_completion_tokens: row.try_get("max_completion_tokens").ok(),
            input_modalities: serde_json::from_str(&input_mod_json)
                .unwrap_or_else(|_| vec!["text".to_string()]),
            output_modalities: serde_json::from_str(&output_mod_json)
                .unwrap_or_else(|_| vec!["text".to_string()]),
            supported_parameters: serde_json::from_str(&supported_json).unwrap_or_default(),
            pricing: serde_json::from_str(&pricing_json).unwrap_or_default(),
            architecture: row.try_get("architecture").ok(),
            tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            quirks: serde_json::from_str(&quirks_json).unwrap_or_default(),
            connectivity: None,
        })
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

pub const MODEL_SELECT_COLS: &str = "\
    model_id, model_name, provider, base_url, description, is_active, \
    IFNULL(context_window, 128000) AS context_window, max_completion_tokens, architecture, \
    IFNULL(CAST(input_modalities AS CHAR), '[\"text\"]') AS input_modalities_json, \
    IFNULL(CAST(output_modalities AS CHAR), '[\"text\"]') AS output_modalities_json, \
    IFNULL(CAST(supported_parameters AS CHAR), '[]') AS supported_parameters_json, \
    IFNULL(CAST(pricing AS CHAR), '{}') AS pricing_json, \
    IFNULL(CAST(tags AS CHAR), '[]') AS tags_json, \
    IFNULL(CAST(quirks AS CHAR), '{}') AS quirks_json";
const MODEL_LIST_SELECT_COLS: &str = "\
    model_id, model_name, provider, description, is_active, \
    IFNULL(context_window, 128000) AS context_window, max_completion_tokens, architecture";
const MAX_MODEL_LIST_ROWS: i64 = 200;

#[async_trait]
impl ModelService for DatabaseModelService {
    async fn create_model(
        &self,
        user_id: String,
        request: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT model_id FROM infra_llm_models WHERE model_name = ? AND provider = ?")
                .bind(&request.name)
                .bind(&request.provider)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        if existing.is_some() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "Model '{}' ({}) already exists",
                    request.name, request.provider
                ),
            ));
        }

        let model_id = Uuid::new_v4().to_string();
        let encrypted_key = self
            .encryptor
            .encrypt(&request.api_key)
            .map_err(internal_error)?;
        let base_url = request
            .base_url
            .or_else(|| resolve_provider_base_url(&request.provider));

        let conn_result = validate_connectivity(
            &request.provider,
            &request.name,
            &request.api_key,
            base_url.as_deref(),
        )
        .await;
        let is_active: i16 = if conn_result.is_none() { 1 } else { 0 };

        let input_mod = serde_json::to_string(&request.input_modalities)
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let output_mod = serde_json::to_string(&request.output_modalities)
            .unwrap_or_else(|_| r#"["text"]"#.to_string());
        let supported = serde_json::to_string(&request.supported_parameters)
            .unwrap_or_else(|_| "[]".to_string());
        let pricing = serde_json::to_string(&request.pricing).unwrap_or_else(|_| "{}".to_string());
        let tags = serde_json::to_string(&request.tags).unwrap_or_else(|_| "[]".to_string());
        let quirks = request
            .quirks
            .as_ref()
            .map(|q| serde_json::to_string(q).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        query(
            "INSERT INTO infra_llm_models \
             (model_id, model_name, provider, api_key_encrypted, base_url, description, \
              is_active, context_window, max_completion_tokens, input_modalities, output_modalities, \
              supported_parameters, pricing, architecture, tags, quirks, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&model_id)
        .bind(&request.name)
        .bind(&request.provider)
        .bind(&encrypted_key)
        .bind(&base_url)
        .bind(&request.description)
        .bind(is_active)
        .bind(request.context_window.unwrap_or(128000))
        .bind(request.max_completion_tokens)
        .bind(&input_mod)
        .bind(&output_mod)
        .bind(&supported)
        .bind(&pricing)
        .bind(&request.architecture)
        .bind(&tags)
        .bind(&quirks)
        .bind(&user_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_id = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&model_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(row)?;
        record.connectivity = Some(conn_result.unwrap_or_else(|| "ok".to_string()));
        Ok(record)
    }

    async fn list_models(
        &self,
        _user_id: String,
        is_admin: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = if is_admin {
            format!(
                "SELECT {} FROM infra_llm_models ORDER BY provider, model_name LIMIT {}",
                MODEL_LIST_SELECT_COLS, MAX_MODEL_LIST_ROWS
            )
        } else {
            format!(
                "SELECT {} FROM infra_llm_models WHERE is_active = 1 ORDER BY provider, model_name LIMIT {}",
                MODEL_LIST_SELECT_COLS, MAX_MODEL_LIST_ROWS
            )
        };
        let rows = query(&sql).fetch_all(&pool).await.map_err(internal_error)?;

        let mut models = Vec::with_capacity(rows.len());
        for row in rows {
            let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);
            models.push(ModelListItem {
                model_id: row.try_get("model_id").map_err(internal_error)?,
                name: row.try_get("model_name").map_err(internal_error)?,
                provider: row.try_get("provider").map_err(internal_error)?,
                description: row.try_get("description").ok(),
                is_active: is_active_int != 0,
                context_window: row.try_get("context_window").unwrap_or(128000),
                max_completion_tokens: row.try_get("max_completion_tokens").ok(),
                architecture: row.try_get("architecture").ok(),
            });
        }
        Ok(models)
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&sql)
            .bind(&model_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;
        Self::model_record_from_row(row)
    }

    async fn update_model(
        &self,
        model_name: String,
        request: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let existing =
            query("SELECT model_id, base_url FROM infra_llm_models WHERE model_name = ?")
                .bind(&model_name)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
        let existing = existing.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;
        let _model_id: String = existing.try_get("model_id").map_err(internal_error)?;

        let mut conn_result: Option<String> = None;

        if let Some(api_key) = &request.api_key {
            let encrypted = self.encryptor.encrypt(api_key).map_err(internal_error)?;
            let base_url: Option<String> = request
                .base_url
                .clone()
                .or_else(|| existing.try_get("base_url").ok());
            let check = validate_connectivity("", &model_name, api_key, base_url.as_deref()).await;

            query("UPDATE infra_llm_models SET api_key_encrypted = ?, updated_at = NOW() WHERE model_name = ?")
                .bind(&encrypted)
                .bind(&model_name)
                .execute(&pool)
                .await
                .map_err(internal_error)?;

            if request.is_active.is_none() {
                let active: i16 = if check.is_none() { 1 } else { 0 };
                query("UPDATE infra_llm_models SET is_active = ? WHERE model_name = ?")
                    .bind(active)
                    .bind(&model_name)
                    .execute(&pool)
                    .await
                    .map_err(internal_error)?;
            }
            conn_result = Some(check.unwrap_or_else(|| "ok".to_string()));
        }

        macro_rules! update_field {
            ($field:ident, $col:expr) => {
                if let Some(val) = &request.$field {
                    let sql = format!("UPDATE infra_llm_models SET {} = ?, updated_at = NOW() WHERE model_name = ?", $col);
                    query(&sql).bind(val).bind(&model_name).execute(&pool).await.map_err(internal_error)?;
                }
            };
            ($field:ident, $col:expr, json) => {
                if let Some(val) = &request.$field {
                    let json_str = serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string());
                    let sql = format!("UPDATE infra_llm_models SET {} = ?, updated_at = NOW() WHERE model_name = ?", $col);
                    query(&sql).bind(&json_str).bind(&model_name).execute(&pool).await.map_err(internal_error)?;
                }
            };
        }
        update_field!(base_url, "base_url");
        update_field!(description, "description");
        update_field!(context_window, "context_window");
        update_field!(max_completion_tokens, "max_completion_tokens");
        update_field!(architecture, "architecture");
        update_field!(input_modalities, "input_modalities", json);
        update_field!(output_modalities, "output_modalities", json);
        update_field!(supported_parameters, "supported_parameters", json);
        update_field!(pricing, "pricing", json);
        update_field!(tags, "tags", json);
        update_field!(quirks, "quirks", json);

        if let Some(active) = request.is_active {
            let val: i16 = if active { 1 } else { 0 };
            query("UPDATE infra_llm_models SET is_active = ?, updated_at = NOW() WHERE model_name = ?")
                .bind(val)
                .bind(&model_name)
                .execute(&pool)
                .await
                .map_err(internal_error)?;
        }

        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let row = query(&sql)
            .bind(&model_name)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(row)?;
        record.connectivity = conn_result;
        Ok(record)
    }

    async fn delete_model(
        &self,
        model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let existing = query("SELECT model_id FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        if existing.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            ));
        }
        query("DELETE FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        Ok(())
    }

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let row = query("SELECT api_key_encrypted, provider, base_url FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Model '{}' not found", model_name),
            )
        })?;

        let encrypted: String = row.try_get("api_key_encrypted").map_err(internal_error)?;
        let provider: String = row.try_get("provider").map_err(internal_error)?;
        let base_url: Option<String> = row.try_get("base_url").ok();

        let api_key = self.encryptor.decrypt(&encrypted).map_err(internal_error)?;
        let check =
            validate_connectivity(&provider, &model_name, &api_key, base_url.as_deref()).await;

        let is_active: i16 = if check.is_none() { 1 } else { 0 };
        query("UPDATE infra_llm_models SET is_active = ?, updated_at = NOW() WHERE model_name = ?")
            .bind(is_active)
            .bind(&model_name)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        let sql = format!(
            "SELECT {} FROM infra_llm_models WHERE model_name = ?",
            MODEL_SELECT_COLS
        );
        let result_row = query(&sql)
            .bind(&model_name)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let mut record = Self::model_record_from_row(result_row)?;
        record.connectivity = Some(check.unwrap_or_else(|| "ok".to_string()));
        Ok(record)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn resolve_provider_base_url(provider: &str) -> Option<String> {
    match provider {
        "openai" => Some("https://api.openai.com/v1".to_string()),
        "anthropic" => None,
        _ => None,
    }
}

pub async fn validate_connectivity(
    provider: &str,
    model_name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Option<String> {
    if provider == "mock" {
        return None;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Some(format!("Client error: {}", e)),
    };

    let result = if provider == "anthropic" && base_url.is_none() {
        client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": model_name,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
    } else {
        let url = base_url
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/');
        client
            .post(format!("{}/chat/completions", url))
            .header("authorization", format!("Bearer {}", api_key))
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": model_name,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
    };

    match result {
        Ok(resp) if resp.status().as_u16() < 400 => None,
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")?
                        .get("message")?
                        .as_str()
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            Some(format!("HTTP {}: {}", status, detail))
        }
        Err(e) => Some(format!("Connection failed: {}", e)),
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredModelService;

#[async_trait]
impl ModelService for UnconfiguredModelService {
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn list_models(
        &self,
        _: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn get_model(&self, _: String) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
    async fn check_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("model service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModelCreateRequest {
    pub name: String,
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    #[serde(default = "default_text_vec")]
    pub input_modalities: Vec<String>,
    #[serde(default = "default_text_vec")]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub pricing: PricingData,
    pub architecture: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub quirks: Option<QuirksData>,
}

fn default_text_vec() -> Vec<String> {
    vec!["text".to_string()]
}

#[derive(Deserialize)]
pub struct ModelUpdateRequest {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
    pub pricing: Option<PricingData>,
    pub architecture: Option<String>,
    pub tags: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub quirks: Option<QuirksData>,
}

#[derive(Serialize, PartialEq)]
pub struct ModelResponse {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub base_url: Option<String>,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub supported_parameters: Vec<String>,
    pub pricing: PricingData,
    pub architecture: Option<String>,
    pub tags: Vec<String>,
    pub quirks: QuirksData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<String>,
}

#[derive(Serialize, PartialEq)]
pub struct ModelListItemResponse {
    pub model_id: String,
    pub name: String,
    pub provider: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub context_window: i32,
    pub max_completion_tokens: Option<i32>,
    pub architecture: Option<String>,
}

impl From<ModelRecord> for ModelResponse {
    fn from(r: ModelRecord) -> Self {
        Self {
            model_id: r.model_id,
            name: r.name,
            provider: r.provider,
            base_url: r.base_url,
            description: r.description,
            is_active: r.is_active,
            context_window: r.context_window,
            max_completion_tokens: r.max_completion_tokens,
            input_modalities: r.input_modalities,
            output_modalities: r.output_modalities,
            supported_parameters: r.supported_parameters,
            pricing: r.pricing,
            architecture: r.architecture,
            tags: r.tags,
            quirks: r.quirks,
            connectivity: r.connectivity,
        }
    }
}

impl From<ModelListItem> for ModelListItemResponse {
    fn from(r: ModelListItem) -> Self {
        Self {
            model_id: r.model_id,
            name: r.name,
            provider: r.provider,
            description: r.description,
            is_active: r.is_active,
            context_window: r.context_window,
            max_completion_tokens: r.max_completion_tokens,
            architecture: r.architecture,
        }
    }
}
