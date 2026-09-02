use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, query};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::registry_payload::{
    RegistryStatus, canonical_serialize, exact_id_string, exact_non_empty_markdown_string,
    exact_non_empty_string, parse_registry_status, reject_secret_like_json,
};
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, error_response_coded, internal_error,
    is_duplicate_key_error,
};

const BINDING_ID_INSERT_MAX_ATTEMPTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentBindingOwnerScope {
    owner_user_id: String,
    /// Opaque exact principal partition. First-party and each provider
    /// subject receive distinct values even when they map to the same user.
    principal_scope_id: String,
}

impl AgentBindingOwnerScope {
    pub fn for_internal_user(user_id: &str) -> Self {
        Self {
            owner_user_id: user_id.to_string(),
            principal_scope_id: "internal".to_string(),
        }
    }

    pub fn from_principal(principal: &crate::AuthPrincipal) -> Self {
        match &principal.origin {
            crate::AuthPrincipalOrigin::Internal => {
                Self::for_internal_user(&principal.user.user_id)
            }
            crate::AuthPrincipalOrigin::ProviderAuthorizedRequest(context) => {
                let mut hasher = Sha256::new();
                hasher.update(b"astra.agent-binding-principal.v1\0");
                for value in [
                    context.provider_id.as_str(),
                    context.external_subject.as_str(),
                    context.provider_scope_id.as_str(),
                ] {
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value.as_bytes());
                }
                Self {
                    owner_user_id: principal.user.user_id.clone(),
                    principal_scope_id: format!("{:x}", hasher.finalize()),
                }
            }
        }
    }

    fn validate(&self) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        exact_id_string(
            "agent binding owner user",
            &self.owner_user_id,
            128,
            "agent_binding_scope_invalid",
        )?;
        let principal_is_valid = self.principal_scope_id == "internal"
            || (self.principal_scope_id.len() == 64
                && self
                    .principal_scope_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        if !principal_is_valid {
            return Err(internal_error("invalid agent binding principal scope"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBindingPayload {
    pub binding_name: String,
    pub agent_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBindingCreateRequestData {
    pub idempotency_key: String,
    pub binding: AgentBindingPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBindingStatus {
    Active,
    Disabled,
    Invalid,
}

impl AgentBindingStatus {
    fn from_registry_status(status: RegistryStatus) -> Self {
        match status {
            RegistryStatus::Active => Self::Active,
            RegistryStatus::Disabled => Self::Disabled,
            RegistryStatus::Invalid => Self::Invalid,
        }
    }

    fn from_db_value(raw: &str) -> Result<Self, (StatusCode, Json<ErrorResponse>)> {
        parse_registry_status("agent binding", raw, "agent_binding_status_invalid")
            .map(Self::from_registry_status)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentBindingRecord {
    pub id: String,
    pub owner_user_id: String,
    pub principal_scope_id: String,
    pub binding_name: String,
    pub idempotency_key: String,
    pub status: AgentBindingStatus,
    pub agent_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

const AGENT_BINDING_COLUMNS: &str = "id, owner_user_id, principal_scope_id, binding_name, idempotency_key, status, agent_md, \
     metadata_json, binding_schema_version, created_at, disabled_at";

impl AgentBindingRecord {
    pub fn payload(&self) -> AgentBindingPayload {
        AgentBindingPayload {
            binding_name: self.binding_name.clone(),
            agent_md: self.agent_md.clone(),
            metadata: self.metadata.clone(),
            binding_schema_version: self.binding_schema_version.clone(),
        }
    }
}

#[async_trait]
pub trait AgentBindingService: Send + Sync {
    async fn create_binding(
        &self,
        scope: AgentBindingOwnerScope,
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn disable_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Default)]
pub struct UnconfiguredAgentBindingService;

#[async_trait]
impl AgentBindingService for UnconfiguredAgentBindingService {
    async fn create_binding(
        &self,
        _scope: AgentBindingOwnerScope,
        _request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "agent binding service not configured",
            "agent_binding_unconfigured",
        ))
    }

    async fn get_binding(
        &self,
        _scope: AgentBindingOwnerScope,
        _id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "agent binding service not configured",
            "agent_binding_unconfigured",
        ))
    }

    async fn disable_binding(
        &self,
        _scope: AgentBindingOwnerScope,
        _id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "agent binding service not configured",
            "agent_binding_unconfigured",
        ))
    }
}

#[derive(Default)]
pub struct InMemoryAgentBindingService {
    records: RwLock<HashMap<String, StoredBinding>>,
    by_name: RwLock<HashMap<(AgentBindingOwnerScope, String), String>>,
    by_idempotency_key: RwLock<HashMap<(AgentBindingOwnerScope, String), String>>,
}

#[derive(Clone)]
struct StoredBinding {
    record: AgentBindingRecord,
    payload: String,
}

impl InMemoryAgentBindingService {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentBindingService for InMemoryAgentBindingService {
    async fn create_binding(
        &self,
        scope: AgentBindingOwnerScope,
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_agent_binding_create(&request)?;
        let payload = canonical_serialize(&request.binding)?;

        let mut records = self.records.write().expect("agent binding lock poisoned");
        let mut by_name = self
            .by_name
            .write()
            .expect("agent binding name lock poisoned");
        let mut by_idempotency_key = self
            .by_idempotency_key
            .write()
            .expect("agent binding idempotency lock poisoned");

        if let Some(id) = by_idempotency_key.get(&(scope.clone(), request.idempotency_key.clone()))
            && let Some(existing) = records.get(id)
        {
            if existing.payload == payload {
                return Ok(existing.record.clone());
            }
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "same idempotency key exists with different binding payload",
                "agent_binding_idempotency_conflict",
            ));
        }

        if let Some(id) = by_name.get(&(scope.clone(), request.binding.binding_name.clone()))
            && let Some(existing) = records.get(id)
        {
            if existing.payload == payload
                && existing.record.idempotency_key == request.idempotency_key
            {
                return Ok(existing.record.clone());
            }
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "same binding name exists with different payload",
                "agent_binding_conflict",
            ));
        }

        let now = chrono::Utc::now().naive_utc().to_string();
        let mut id = None;
        for _ in 0..BINDING_ID_INSERT_MAX_ATTEMPTS {
            let candidate = new_binding_id();
            if !records.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }
        let id = id.ok_or_else(|| internal_error("agent binding id collision retry exhausted"))?;
        let record = AgentBindingRecord {
            id: id.clone(),
            owner_user_id: scope.owner_user_id.clone(),
            principal_scope_id: scope.principal_scope_id.clone(),
            binding_name: request.binding.binding_name.clone(),
            idempotency_key: request.idempotency_key.clone(),
            status: AgentBindingStatus::Active,
            agent_md: request.binding.agent_md,
            metadata: request.binding.metadata,
            binding_schema_version: request.binding.binding_schema_version,
            created_at: now,
            disabled_at: None,
        };
        by_name.insert((scope.clone(), record.binding_name.clone()), id.clone());
        by_idempotency_key.insert((scope, record.idempotency_key.clone()), id.clone());
        records.insert(
            id,
            StoredBinding {
                record: record.clone(),
                payload,
            },
        );
        Ok(record)
    }

    async fn get_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_binding_id_for_lookup(&id)?;
        self.records
            .read()
            .expect("agent binding lock poisoned")
            .get(&id)
            .filter(|stored| {
                stored.record.owner_user_id == scope.owner_user_id
                    && stored.record.principal_scope_id == scope.principal_scope_id
            })
            .map(|stored| stored.record.clone())
            .ok_or_else(agent_binding_not_found)
    }

    async fn disable_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_binding_id_for_lookup(&id)?;
        let mut records = self.records.write().expect("agent binding lock poisoned");
        let stored = records
            .get_mut(&id)
            .filter(|stored| {
                stored.record.owner_user_id == scope.owner_user_id
                    && stored.record.principal_scope_id == scope.principal_scope_id
            })
            .ok_or_else(agent_binding_not_found)?;
        stored.record.status = AgentBindingStatus::Disabled;
        stored.record.disabled_at = Some(chrono::Utc::now().naive_utc().to_string());
        Ok(stored.record.clone())
    }
}

#[derive(Clone)]
pub struct DatabaseAgentBindingService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAgentBindingService {
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
            "DatabaseAgentBindingService",
            &self.matrixone,
        )
    }
}

#[async_trait]
impl AgentBindingService for DatabaseAgentBindingService {
    async fn create_binding(
        &self,
        scope: AgentBindingOwnerScope,
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_agent_binding_create(&request)?;
        let payload = canonical_serialize(&request.binding)?;
        let pool = self.get_pool().await.map_err(internal_error)?;

        if let Some(existing) =
            load_binding_by_idempotency_key(&pool, &scope, &request.idempotency_key).await?
        {
            if canonical_serialize(&existing.payload())? == payload {
                return Ok(existing);
            }
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "same idempotency key exists with different binding payload",
                "agent_binding_idempotency_conflict",
            ));
        }

        if let Some(existing) =
            load_binding_by_name(&pool, &scope, &request.binding.binding_name).await?
        {
            if canonical_serialize(&existing.payload())? == payload
                && existing.idempotency_key == request.idempotency_key
            {
                return Ok(existing);
            }
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "same binding name exists with different payload",
                "agent_binding_conflict",
            ));
        }

        let metadata_json = optional_json_string(request.binding.metadata.as_ref())?;

        for attempt in 0..BINDING_ID_INSERT_MAX_ATTEMPTS {
            let id = new_binding_id();
            let insert_result = query(
                "INSERT INTO agent_bindings \
                 (id, owner_user_id, principal_scope_id, binding_name, idempotency_key, status, agent_md, \
                  metadata_json, binding_schema_version, created_at) \
                 VALUES (?, ?, ?, ?, ?, 'active', ?, ?, ?, NOW(6))",
            )
            .bind(&id)
            .bind(&scope.owner_user_id)
            .bind(&scope.principal_scope_id)
            .bind(&request.binding.binding_name)
            .bind(&request.idempotency_key)
            .bind(&request.binding.agent_md)
            .bind(metadata_json.clone())
            .bind(&request.binding.binding_schema_version)
            .execute(&pool)
            .await;

            match insert_result {
                Ok(_) => {
                    return load_binding_row(&pool, &scope, &id)
                        .await?
                        .ok_or_else(agent_binding_not_found);
                }
                Err(error) if is_duplicate_key_error(&error) => {
                    if binding_id_exists(&pool, &id).await? {
                        if attempt + 1 == BINDING_ID_INSERT_MAX_ATTEMPTS {
                            return Err(internal_error(
                                "agent binding id collision retry exhausted",
                            ));
                        }
                        continue;
                    }
                    if let Some(existing) =
                        load_binding_by_idempotency_key(&pool, &scope, &request.idempotency_key)
                            .await?
                    {
                        if canonical_serialize(&existing.payload())? == payload {
                            return Ok(existing);
                        }
                        return Err(error_response_coded(
                            StatusCode::CONFLICT,
                            "same idempotency key exists with different binding payload",
                            "agent_binding_idempotency_conflict",
                        ));
                    }
                    if let Some(existing) =
                        load_binding_by_name(&pool, &scope, &request.binding.binding_name).await?
                    {
                        if canonical_serialize(&existing.payload())? == payload
                            && existing.idempotency_key == request.idempotency_key
                        {
                            return Ok(existing);
                        }
                        return Err(error_response_coded(
                            StatusCode::CONFLICT,
                            "same binding name exists with different payload",
                            "agent_binding_conflict",
                        ));
                    }
                    return Err(internal_error(error));
                }
                Err(error) => return Err(internal_error(error)),
            }
        }

        Err(internal_error("agent binding id collision retry exhausted"))
    }

    async fn get_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_binding_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        load_binding_row(&pool, &scope, &id)
            .await?
            .ok_or_else(agent_binding_not_found)
    }

    async fn disable_binding(
        &self,
        scope: AgentBindingOwnerScope,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        scope.validate()?;
        validate_binding_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        query(
            "UPDATE agent_bindings \
             SET status = 'disabled', disabled_at = COALESCE(disabled_at, NOW(6)), updated_at = NOW(6) \
             WHERE id = ? AND owner_user_id = ? AND principal_scope_id = ?",
        )
        .bind(&id)
        .bind(&scope.owner_user_id)
        .bind(&scope.principal_scope_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;
        load_binding_row(&pool, &scope, &id)
            .await?
            .ok_or_else(agent_binding_not_found)
    }
}

pub fn validate_agent_binding_create(
    request: &AgentBindingCreateRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_id_string(
        "idempotency_key",
        &request.idempotency_key,
        255,
        "agent_binding_invalid",
    )?;
    validate_agent_binding_payload(&request.binding)
}

pub fn validate_agent_binding_payload(
    payload: &AgentBindingPayload,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_id_string(
        "binding_name",
        &payload.binding_name,
        255,
        "agent_binding_invalid",
    )?;
    exact_non_empty_markdown_string("agent_md", &payload.agent_md, "agent_binding_invalid")?;
    let max_agent_md_bytes = astra_config::runtime_config::RuntimeConfig::cached()
        .agent_binding_registry
        .max_agent_md_bytes as usize;
    if payload.agent_md.len() > max_agent_md_bytes {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("agent_md must not exceed {max_agent_md_bytes} bytes"),
            "agent_binding_invalid",
        ));
    }
    exact_non_empty_string(
        "binding_schema_version",
        &payload.binding_schema_version,
        "agent_binding_invalid",
    )?;
    if payload.binding_schema_version != "v1" {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "binding_schema_version must be v1",
            "agent_binding_invalid",
        ));
    }
    if let Some(metadata) = &payload.metadata {
        reject_secret_like_json("metadata", metadata, "agent_binding_invalid")?;
    }
    Ok(())
}

fn validate_binding_id_for_lookup(id: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_non_empty_string("id", id, "agent_binding_invalid")?;
    if !id.starts_with("ab_") || id.len() > 64 || id.contains('/') || id.contains('\\') {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "id must be an Astra agent binding id",
            "agent_binding_invalid",
        ));
    }
    Ok(())
}

fn new_binding_id() -> String {
    format!("ab_{}", uuid::Uuid::now_v7())
}

fn agent_binding_not_found() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::NOT_FOUND,
        "agent binding not found",
        "agent_binding_not_found",
    )
}

fn optional_json_string(
    value: Option<&serde_json::Value>,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    value.map(serde_json::to_string).transpose().map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "metadata must be valid JSON",
            "agent_binding_invalid",
        )
    })
}

async fn load_binding_row(
    pool: &sqlx::Pool<MySql>,
    scope: &AgentBindingOwnerScope,
    id: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let sql = format!(
        "SELECT {AGENT_BINDING_COLUMNS} FROM agent_bindings \
         WHERE id = ? AND owner_user_id = ? AND principal_scope_id = ?"
    );
    let row = query(&sql)
        .bind(id)
        .bind(&scope.owner_user_id)
        .bind(&scope.principal_scope_id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

async fn binding_id_exists(
    pool: &sqlx::Pool<MySql>,
    id: &str,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    query("SELECT 1 FROM agent_bindings WHERE id = ? LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.is_some())
        .map_err(internal_error)
}

async fn load_binding_by_idempotency_key(
    pool: &sqlx::Pool<MySql>,
    scope: &AgentBindingOwnerScope,
    idempotency_key: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let sql = format!(
        "SELECT {AGENT_BINDING_COLUMNS} FROM agent_bindings WHERE owner_user_id = ? AND principal_scope_id = ? AND idempotency_key = ?"
    );
    let row = query(&sql)
        .bind(&scope.owner_user_id)
        .bind(&scope.principal_scope_id)
        .bind(idempotency_key)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

async fn load_binding_by_name(
    pool: &sqlx::Pool<MySql>,
    scope: &AgentBindingOwnerScope,
    binding_name: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let sql = format!(
        "SELECT {AGENT_BINDING_COLUMNS} FROM agent_bindings WHERE owner_user_id = ? AND principal_scope_id = ? AND binding_name = ?"
    );
    let row = query(&sql)
        .bind(&scope.owner_user_id)
        .bind(&scope.principal_scope_id)
        .bind(binding_name)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

fn agent_binding_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
    let metadata_json: Option<String> = row.try_get("metadata_json").map_err(internal_error)?;
    let metadata = metadata_json
        .as_deref()
        .map(|raw| {
            serde_json::from_str(raw).map_err(|error| {
                internal_error(format!("invalid agent_bindings.metadata_json: {error}"))
            })
        })
        .transpose()?;
    Ok(AgentBindingRecord {
        id: row.try_get("id").map_err(internal_error)?,
        owner_user_id: row.try_get("owner_user_id").map_err(internal_error)?,
        principal_scope_id: row.try_get("principal_scope_id").map_err(internal_error)?,
        binding_name: row.try_get("binding_name").map_err(internal_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(internal_error)?,
        status: AgentBindingStatus::from_db_value(
            &row.try_get::<String, _>("status").map_err(internal_error)?,
        )?,
        agent_md: row.try_get("agent_md").map_err(internal_error)?,
        metadata,
        binding_schema_version: row
            .try_get("binding_schema_version")
            .map_err(internal_error)?,
        created_at: row_datetime_string(&row, "created_at")?,
        disabled_at: row_datetime_string_opt(&row, "disabled_at")?,
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

    fn owner_scope() -> AgentBindingOwnerScope {
        AgentBindingOwnerScope::for_internal_user("user-a")
    }

    fn valid_request() -> AgentBindingCreateRequestData {
        AgentBindingCreateRequestData {
            idempotency_key: "key-01".into(),
            binding: AgentBindingPayload {
                binding_name: "binding-01".into(),
                agent_md: "You are a test agent.".into(),
                metadata: None,
                binding_schema_version: "v1".into(),
            },
        }
    }

    #[test]
    fn agent_binding_status_parser_fails_closed_on_unknown_status() {
        let err = AgentBindingStatus::from_db_value("paused")
            .expect_err("unknown persisted status must not become active");
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("agent_binding_status_invalid")
        );
    }

    #[tokio::test]
    async fn in_memory_binding_is_idempotent_for_same_payload() {
        let svc = InMemoryAgentBindingService::new();
        let first = svc
            .create_binding(owner_scope(), valid_request())
            .await
            .unwrap();
        let second = svc
            .create_binding(owner_scope(), valid_request())
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert!(first.id.starts_with("ab_"));
    }

    #[tokio::test]
    async fn in_memory_binding_name_and_idempotency_are_owner_scoped() {
        let svc = InMemoryAgentBindingService::new();
        let request = valid_request();
        let a = svc
            .create_binding(
                AgentBindingOwnerScope::for_internal_user("user-a"),
                request.clone(),
            )
            .await
            .unwrap();
        let b = svc
            .create_binding(AgentBindingOwnerScope::for_internal_user("user-b"), request)
            .await
            .unwrap();

        assert_ne!(a.id, b.id);
        assert_eq!(a.binding_name, b.binding_name);
        assert_eq!(a.idempotency_key, b.idempotency_key);
    }

    #[tokio::test]
    async fn foreign_owner_cannot_get_disable_or_use_binding() {
        let svc = InMemoryAgentBindingService::new();
        let owner = AgentBindingOwnerScope::for_internal_user("user-a");
        let foreign = AgentBindingOwnerScope::for_internal_user("user-b");
        let created = svc
            .create_binding(owner.clone(), valid_request())
            .await
            .unwrap();

        let get_error = svc
            .get_binding(foreign.clone(), created.id.clone())
            .await
            .unwrap_err();
        assert_eq!(get_error.0, StatusCode::NOT_FOUND);
        let disable_error = svc
            .disable_binding(foreign, created.id.clone())
            .await
            .unwrap_err();
        assert_eq!(disable_error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            svc.get_binding(owner, created.id).await.unwrap().status,
            AgentBindingStatus::Active
        );
    }

    #[tokio::test]
    async fn provider_subjects_are_distinct_binding_tenants() {
        fn principal(subject: &str) -> crate::AuthPrincipal {
            crate::AuthPrincipal {
                user: crate::AuthUserRecord {
                    user_id: "mapped-user".to_string(),
                    username: "mapped-user".to_string(),
                    email: "mapped@example.test".to_string(),
                    display_name: None,
                },
                session_id: None,
                origin: crate::AuthPrincipalOrigin::ProviderAuthorizedRequest(
                    crate::AuthProviderAuthorizedRequestContext {
                        provider_id: "provider-a".to_string(),
                        external_subject: subject.to_string(),
                        provider_scope_id: "workspace-a".to_string(),
                        request_authorization_id: format!("authorization-{subject}"),
                        edge_agent_id: None,
                    },
                ),
            }
        }

        let svc = InMemoryAgentBindingService::new();
        let first_scope = AgentBindingOwnerScope::from_principal(&principal("subject-a"));
        let second_scope = AgentBindingOwnerScope::from_principal(&principal("subject-b"));
        assert_ne!(first_scope, second_scope);
        let first = svc
            .create_binding(first_scope.clone(), valid_request())
            .await
            .unwrap();
        let second = svc
            .create_binding(second_scope.clone(), valid_request())
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(svc.get_binding(second_scope, first.id).await.is_err());
    }

    #[tokio::test]
    async fn ownerless_legacy_binding_record_fails_closed() {
        let svc = InMemoryAgentBindingService::new();
        let scope = owner_scope();
        let created = svc
            .create_binding(scope.clone(), valid_request())
            .await
            .unwrap();
        {
            let mut records = svc.records.write().unwrap();
            let stored = records.get_mut(&created.id).unwrap();
            stored.record.owner_user_id.clear();
            stored.record.principal_scope_id.clear();
        }

        let error = svc
            .get_binding(scope, created.id)
            .await
            .expect_err("unscoped persisted records must never be globally visible");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn binding_conflicts_on_same_idempotency_key_different_payload() {
        let svc = InMemoryAgentBindingService::new();
        svc.create_binding(owner_scope(), valid_request())
            .await
            .unwrap();
        let mut changed = valid_request();
        changed.binding.agent_md = "different".into();
        let err = svc
            .create_binding(owner_scope(), changed)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("agent_binding_idempotency_conflict")
        );
    }

    #[tokio::test]
    async fn binding_accepts_multiline_agent_md() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        request.binding.agent_md = "Role\nMission\nOutput contract".into();

        let created = svc.create_binding(owner_scope(), request).await.unwrap();
        assert_eq!(created.status, AgentBindingStatus::Active);
    }

    #[tokio::test]
    async fn binding_rejects_trailing_agent_md_whitespace() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        request.binding.agent_md = "Role\n".into();

        let err = svc
            .create_binding(owner_scope(), request)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }

    #[tokio::test]
    async fn binding_rejects_agent_md_above_configured_byte_limit() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        let max_bytes = astra_config::runtime_config::RuntimeConfig::cached()
            .agent_binding_registry
            .max_agent_md_bytes as usize;
        request.binding.agent_md = "a".repeat(max_bytes.saturating_add(1));
        let err = svc
            .create_binding(owner_scope(), request)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }

    #[tokio::test]
    async fn binding_rejects_runtime_scope_fields_in_metadata() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        request.binding.metadata = Some(serde_json::json!({
            "selected_model": {"model": "gpt-4"},
        }));
        let err = svc
            .create_binding(owner_scope(), request)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }
}
