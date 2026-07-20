use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Row, query};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::registry_payload::{
    RegistryStatus, canonical_serialize, exact_id_string, parse_registry_status,
    reject_secret_like_json, validate_registered_endpoint_url,
};
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, error_response_coded, internal_error,
    is_duplicate_key_error,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
}

impl ModelProtocol {
    pub fn from_wire_value(raw: &str) -> Result<Self, (StatusCode, Json<ErrorResponse>)> {
        match raw {
            "openai_chat_completions" => Ok(Self::OpenAiChatCompletions),
            _ => Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "model gateway protocol is not implemented",
                "model_gateway_protocol_unsupported",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelGatewayStatus {
    Active,
    Disabled,
    Invalid,
}

impl ModelGatewayStatus {
    fn from_registry_status(status: RegistryStatus) -> Self {
        match status {
            RegistryStatus::Active => Self::Active,
            RegistryStatus::Disabled => Self::Disabled,
            RegistryStatus::Invalid => Self::Invalid,
        }
    }

    fn from_db_value(raw: &str) -> Result<Self, (StatusCode, Json<ErrorResponse>)> {
        parse_registry_status("model gateway", raw, "model_gateway_status_invalid")
            .map(Self::from_registry_status)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGatewayCreateRequestData {
    pub id: String,
    pub resolve_url: String,
    pub model_protocol: ModelProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelGatewayRecord {
    pub id: String,
    pub resolve_url: String,
    pub model_protocol: ModelProtocol,
    pub status: ModelGatewayStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

const MODEL_GATEWAY_COLUMNS: &str = "id, resolve_url, model_protocol, status, metadata_json, \
     created_at, updated_at, disabled_at";

#[async_trait]
pub trait ModelGatewayService: Send + Sync {
    async fn create_gateway(
        &self,
        request: ModelGatewayCreateRequestData,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn disable_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Default)]
pub struct UnconfiguredModelGatewayService;

#[async_trait]
impl ModelGatewayService for UnconfiguredModelGatewayService {
    async fn create_gateway(
        &self,
        _request: ModelGatewayCreateRequestData,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "model gateway service not configured",
            "model_gateway_unconfigured",
        ))
    }

    async fn get_gateway(
        &self,
        _id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "model gateway service not configured",
            "model_gateway_unconfigured",
        ))
    }

    async fn disable_gateway(
        &self,
        _id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "model gateway service not configured",
            "model_gateway_unconfigured",
        ))
    }
}

#[derive(Default)]
pub struct InMemoryModelGatewayService {
    records: RwLock<HashMap<String, StoredModelGateway>>,
}

#[derive(Clone)]
struct StoredModelGateway {
    record: ModelGatewayRecord,
    payload: String,
}

impl InMemoryModelGatewayService {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelGatewayService for InMemoryModelGatewayService {
    async fn create_gateway(
        &self,
        request: ModelGatewayCreateRequestData,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_model_gateway_create(&request)?;
        let payload = canonical_serialize(&request)?;
        let mut records = self.records.write().expect("model gateway lock poisoned");
        if let Some(existing) = records.get(&request.id) {
            if existing.payload == payload {
                return Ok(existing.record.clone());
            }
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "same model gateway id exists with different payload",
                "model_gateway_conflict",
            ));
        }

        let now = chrono::Utc::now().naive_utc().to_string();
        let record = ModelGatewayRecord {
            id: request.id.clone(),
            resolve_url: request.resolve_url,
            model_protocol: request.model_protocol,
            status: ModelGatewayStatus::Active,
            metadata: request.metadata,
            created_at: now.clone(),
            updated_at: now,
            disabled_at: None,
        };
        records.insert(
            record.id.clone(),
            StoredModelGateway {
                record: record.clone(),
                payload,
            },
        );
        Ok(record)
    }

    async fn get_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_gateway_id_for_lookup(&id)?;
        self.records
            .read()
            .expect("model gateway lock poisoned")
            .get(&id)
            .map(|stored| stored.record.clone())
            .ok_or_else(model_gateway_not_found)
    }

    async fn disable_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_gateway_id_for_lookup(&id)?;
        let mut records = self.records.write().expect("model gateway lock poisoned");
        let stored = records.get_mut(&id).ok_or_else(model_gateway_not_found)?;
        stored.record.status = ModelGatewayStatus::Disabled;
        stored.record.updated_at = chrono::Utc::now().naive_utc().to_string();
        stored.record.disabled_at = Some(stored.record.updated_at.clone());
        Ok(stored.record.clone())
    }
}

#[derive(Clone)]
pub struct DatabaseModelGatewayService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseModelGatewayService {
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

    async fn get_pool(&self) -> Result<sqlx::Pool<MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseModelGatewayService",
            &self.matrixone,
        )
    }
}

#[async_trait]
impl ModelGatewayService for DatabaseModelGatewayService {
    async fn create_gateway(
        &self,
        request: ModelGatewayCreateRequestData,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_model_gateway_create(&request)?;
        let payload = canonical_serialize(&request)?;
        let metadata_json = optional_json_string(request.metadata.as_ref())?;
        let pool = self.get_pool().await.map_err(internal_error)?;

        let insert_result = query(
            "INSERT INTO model_gateways \
             (id, resolve_url, model_protocol, status, metadata_json, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, NOW(6), NOW(6))",
        )
        .bind(&request.id)
        .bind(&request.resolve_url)
        .bind(model_protocol_wire(&request.model_protocol))
        .bind(metadata_json)
        .execute(&pool)
        .await;

        match insert_result {
            Ok(_) => load_gateway_row(&pool, &request.id)
                .await?
                .ok_or_else(model_gateway_not_found),
            Err(error) if is_duplicate_key_error(&error) => {
                reconcile_duplicate_gateway_insert(&pool, &request.id, &payload).await
            }
            Err(error) => Err(internal_error(error)),
        }
    }

    async fn get_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_gateway_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        load_gateway_row(&pool, &id)
            .await?
            .ok_or_else(model_gateway_not_found)
    }

    async fn disable_gateway(
        &self,
        id: String,
    ) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_gateway_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        query(
            "UPDATE model_gateways \
             SET status = 'disabled', disabled_at = COALESCE(disabled_at, NOW(6)), updated_at = NOW(6) \
             WHERE id = ?",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;
        load_gateway_row(&pool, &id)
            .await?
            .ok_or_else(model_gateway_not_found)
    }
}

fn validate_model_gateway_create(
    request: &ModelGatewayCreateRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_id_string("id", &request.id, 128, "model_gateway_invalid")?;
    validate_registered_endpoint_url("resolve_url", &request.resolve_url, "model_gateway_invalid")?;
    if let Some(metadata) = &request.metadata {
        reject_secret_like_json("metadata", metadata, "model_gateway_invalid")?;
    }
    Ok(())
}

fn validate_gateway_id_for_lookup(id: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_id_string("id", id, 128, "model_gateway_invalid")
}

fn model_gateway_not_found() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::NOT_FOUND,
        "model gateway not found",
        "model_gateway_not_found",
    )
}

fn optional_json_string(
    value: Option<&serde_json::Value>,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    value.map(serde_json::to_string).transpose().map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "metadata must be valid JSON",
            "model_gateway_invalid",
        )
    })
}

fn model_protocol_wire(protocol: &ModelProtocol) -> &'static str {
    match protocol {
        ModelProtocol::OpenAiChatCompletions => "openai_chat_completions",
    }
}

async fn load_gateway_row(
    pool: &sqlx::Pool<MySql>,
    id: &str,
) -> Result<Option<ModelGatewayRecord>, (StatusCode, Json<ErrorResponse>)> {
    let sql = format!("SELECT {MODEL_GATEWAY_COLUMNS} FROM model_gateways WHERE id = ?");
    let row = query(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(model_gateway_from_row).transpose()
}

async fn reconcile_duplicate_gateway_insert(
    pool: &sqlx::Pool<MySql>,
    id: &str,
    payload: &str,
) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
    let Some(existing) = load_gateway_row(pool, id).await? else {
        return Err(internal_error(
            "model gateway duplicate key reported but existing row was not found",
        ));
    };
    if canonical_gateway_payload(&existing)? == payload {
        return Ok(existing);
    }
    Err(error_response_coded(
        StatusCode::CONFLICT,
        "same model gateway id exists with different payload",
        "model_gateway_conflict",
    ))
}

fn model_gateway_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<ModelGatewayRecord, (StatusCode, Json<ErrorResponse>)> {
    let protocol: String = row.try_get("model_protocol").map_err(internal_error)?;
    let metadata_raw: Option<String> = row.try_get("metadata_json").map_err(internal_error)?;
    let metadata = metadata_raw
        .as_deref()
        .map(|raw| {
            serde_json::from_str(raw).map_err(|error| {
                internal_error(format!("invalid model_gateways.metadata_json: {error}"))
            })
        })
        .transpose()?;
    Ok(ModelGatewayRecord {
        id: row.try_get("id").map_err(internal_error)?,
        resolve_url: row.try_get("resolve_url").map_err(internal_error)?,
        model_protocol: ModelProtocol::from_wire_value(&protocol)?,
        status: ModelGatewayStatus::from_db_value(
            &row.try_get::<String, _>("status").map_err(internal_error)?,
        )?,
        metadata,
        created_at: row_datetime_string(&row, "created_at")?,
        updated_at: row_datetime_string(&row, "updated_at")?,
        disabled_at: row_datetime_string_opt(&row, "disabled_at")?,
    })
}

fn canonical_gateway_payload(
    record: &ModelGatewayRecord,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    canonical_serialize(&ModelGatewayCreateRequestData {
        id: record.id.clone(),
        resolve_url: record.resolve_url.clone(),
        model_protocol: record.model_protocol.clone(),
        metadata: record.metadata.clone(),
    })
}

fn row_datetime_string(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<NaiveDateTime, _>(column)
        .map(|v| v.to_string())
        .map_err(internal_error)
}

fn row_datetime_string_opt(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<Option<NaiveDateTime>, _>(column)
        .map(|v| v.map(|dt| dt.to_string()))
        .map_err(internal_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_protocol_wire_shape_matches_public_contract() {
        let encoded =
            serde_json::to_string(&ModelProtocol::OpenAiChatCompletions).expect("serialize");
        assert_eq!(encoded, "\"openai_chat_completions\"");
        let decoded: ModelProtocol =
            serde_json::from_str("\"openai_chat_completions\"").expect("deserialize");
        assert_eq!(decoded, ModelProtocol::OpenAiChatCompletions);
        let err = serde_json::from_str::<ModelProtocol>("\"open_ai_chat_completions\"")
            .expect_err("legacy misspelling must not be accepted");
        assert!(err.is_data());
    }

    #[test]
    fn model_gateway_status_parser_fails_closed_on_unknown_status() {
        let err = ModelGatewayStatus::from_db_value("paused")
            .expect_err("unknown persisted status must not become active");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_status_invalid")
        );
    }

    #[tokio::test]
    async fn in_memory_gateway_is_idempotent_for_same_payload() {
        let svc = InMemoryModelGatewayService::new();
        let request = ModelGatewayCreateRequestData {
            id: "primary".into(),
            resolve_url: "https://models.example.com/resolve".into(),
            model_protocol: ModelProtocol::OpenAiChatCompletions,
            metadata: None,
        };

        let first = svc.create_gateway(request.clone()).await.unwrap();
        let second = svc.create_gateway(request).await.unwrap();

        assert_eq!(first.id, "primary");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn gateway_conflicts_on_same_id_different_payload() {
        let svc = InMemoryModelGatewayService::new();
        svc.create_gateway(ModelGatewayCreateRequestData {
            id: "primary".into(),
            resolve_url: "https://models.example.com/resolve".into(),
            model_protocol: ModelProtocol::OpenAiChatCompletions,
            metadata: None,
        })
        .await
        .unwrap();

        let err = svc
            .create_gateway(ModelGatewayCreateRequestData {
                id: "primary".into(),
                resolve_url: "https://models.example.com/other".into(),
                model_protocol: ModelProtocol::OpenAiChatCompletions,
                metadata: None,
            })
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.error_code.as_deref(), Some("model_gateway_conflict"));
    }

    #[tokio::test]
    async fn gateway_rejects_credential_bearing_url() {
        let svc = InMemoryModelGatewayService::new();
        let err = svc
            .create_gateway(ModelGatewayCreateRequestData {
                id: "primary".into(),
                resolve_url: "https://models.example.com/resolve?token=secret".into(),
                model_protocol: ModelProtocol::OpenAiChatCompletions,
                metadata: None,
            })
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("model_gateway_invalid"));
    }
}
