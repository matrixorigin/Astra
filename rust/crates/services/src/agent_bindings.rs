use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Row, query};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use crate::registry_payload::{
    RegistryStatus, canonical_serialize, exact_id_string, exact_non_empty_string,
    parse_registry_status, reject_secret_like_json, validate_registered_endpoint_url,
};
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, error_response_coded, internal_error,
    is_duplicate_key_error,
};

const BINDING_ID_INSERT_MAX_ATTEMPTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityServerType {
    Mcp,
    Skill,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityServerTransport {
    StreamableHttp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityServerEndpoint {
    pub id: String,
    #[serde(rename = "type")]
    pub server_type: CapabilityServerType,
    pub transport: CapabilityServerTransport,
    pub endpoint_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    McpGateway,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    pub tool_mode: ToolMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBindingPayload {
    pub binding_name: String,
    pub agent_md: String,
    pub capability_servers: Vec<CapabilityServerEndpoint>,
    pub runtime_policy: RuntimePolicy,
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
    pub binding_name: String,
    pub idempotency_key: String,
    pub status: AgentBindingStatus,
    pub agent_md: String,
    pub capability_servers: Vec<CapabilityServerEndpoint>,
    pub runtime_policy: RuntimePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

impl AgentBindingRecord {
    pub fn payload(&self) -> AgentBindingPayload {
        AgentBindingPayload {
            binding_name: self.binding_name.clone(),
            agent_md: self.agent_md.clone(),
            capability_servers: self.capability_servers.clone(),
            runtime_policy: self.runtime_policy.clone(),
            metadata: self.metadata.clone(),
            binding_schema_version: self.binding_schema_version.clone(),
        }
    }
}

#[async_trait]
pub trait AgentBindingService: Send + Sync {
    async fn create_binding(
        &self,
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_binding(
        &self,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn disable_binding(
        &self,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Default)]
pub struct UnconfiguredAgentBindingService;

#[async_trait]
impl AgentBindingService for UnconfiguredAgentBindingService {
    async fn create_binding(
        &self,
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
    by_name: RwLock<HashMap<String, String>>,
    by_idempotency_key: RwLock<HashMap<String, String>>,
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
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
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

        if let Some(id) = by_idempotency_key.get(&request.idempotency_key)
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

        if let Some(id) = by_name.get(&request.binding.binding_name)
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
            binding_name: request.binding.binding_name.clone(),
            idempotency_key: request.idempotency_key.clone(),
            status: AgentBindingStatus::Active,
            agent_md: request.binding.agent_md,
            capability_servers: request.binding.capability_servers,
            runtime_policy: request.binding.runtime_policy,
            metadata: request.binding.metadata,
            binding_schema_version: request.binding.binding_schema_version,
            created_at: now,
            disabled_at: None,
        };
        by_name.insert(record.binding_name.clone(), id.clone());
        by_idempotency_key.insert(record.idempotency_key.clone(), id.clone());
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
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_binding_id_for_lookup(&id)?;
        self.records
            .read()
            .expect("agent binding lock poisoned")
            .get(&id)
            .map(|stored| stored.record.clone())
            .ok_or_else(agent_binding_not_found)
    }

    async fn disable_binding(
        &self,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_binding_id_for_lookup(&id)?;
        let mut records = self.records.write().expect("agent binding lock poisoned");
        let stored = records.get_mut(&id).ok_or_else(agent_binding_not_found)?;
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
        request: AgentBindingCreateRequestData,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_agent_binding_create(&request)?;
        let payload = canonical_serialize(&request.binding)?;
        let pool = self.get_pool().await.map_err(internal_error)?;

        if let Some(existing) =
            load_binding_by_idempotency_key(&pool, &request.idempotency_key).await?
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

        if let Some(existing) = load_binding_by_name(&pool, &request.binding.binding_name).await? {
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

        let capability_servers_json = json_string(
            &request.binding.capability_servers,
            "capability_servers_json",
        )?;
        let runtime_policy_json =
            json_string(&request.binding.runtime_policy, "runtime_policy_json")?;
        let metadata_json = optional_json_string(request.binding.metadata.as_ref())?;

        for attempt in 0..BINDING_ID_INSERT_MAX_ATTEMPTS {
            let id = new_binding_id();
            let insert_result = query(
                "INSERT INTO agent_bindings \
                 (id, binding_name, idempotency_key, status, agent_md, capability_servers_json, \
                  runtime_policy_json, metadata_json, binding_schema_version, created_at) \
                 VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?, NOW(6))",
            )
            .bind(&id)
            .bind(&request.binding.binding_name)
            .bind(&request.idempotency_key)
            .bind(&request.binding.agent_md)
            .bind(&capability_servers_json)
            .bind(&runtime_policy_json)
            .bind(metadata_json.clone())
            .bind(&request.binding.binding_schema_version)
            .execute(&pool)
            .await;

            match insert_result {
                Ok(_) => {
                    return load_binding_row(&pool, &id)
                        .await?
                        .ok_or_else(agent_binding_not_found);
                }
                Err(error) if is_duplicate_key_error(&error) => {
                    if load_binding_row(&pool, &id).await?.is_some() {
                        if attempt + 1 == BINDING_ID_INSERT_MAX_ATTEMPTS {
                            return Err(internal_error(
                                "agent binding id collision retry exhausted",
                            ));
                        }
                        continue;
                    }
                    if let Some(existing) =
                        load_binding_by_idempotency_key(&pool, &request.idempotency_key).await?
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
                        load_binding_by_name(&pool, &request.binding.binding_name).await?
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
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_binding_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        load_binding_row(&pool, &id)
            .await?
            .ok_or_else(agent_binding_not_found)
    }

    async fn disable_binding(
        &self,
        id: String,
    ) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_binding_id_for_lookup(&id)?;
        let pool = self.get_pool().await.map_err(internal_error)?;
        query(
            "UPDATE agent_bindings \
             SET status = 'disabled', disabled_at = COALESCE(disabled_at, NOW(6)), updated_at = NOW(6) \
             WHERE id = ?",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;
        load_binding_row(&pool, &id)
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
    exact_non_empty_string("agent_md", &payload.agent_md, "agent_binding_invalid")?;
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
    if payload.capability_servers.is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "capability_servers must not be empty",
            "agent_binding_invalid",
        ));
    }
    if payload.runtime_policy.tool_mode != ToolMode::McpGateway {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "runtime_policy.tool_mode must be mcp_gateway",
            "agent_binding_policy_invalid",
        ));
    }
    if let Some(max_steps) = payload.runtime_policy.max_steps
        && max_steps == 0
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "runtime_policy.max_steps must be positive",
            "agent_binding_policy_invalid",
        ));
    }
    if let Some(max_steps) = payload.runtime_policy.max_steps {
        let max_allowed = astra_config::runtime_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(false);
        if (max_steps as usize) > max_allowed {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                format!(
                    "runtime_policy.max_steps must not exceed server max_turns ({max_allowed})"
                ),
                "agent_binding_policy_invalid",
            ));
        }
    }
    if let Some(metadata) = &payload.metadata {
        reject_secret_like_json("metadata", metadata, "agent_binding_invalid")?;
    }
    validate_capability_servers(&payload.capability_servers)
}

fn validate_capability_servers(
    servers: &[CapabilityServerEndpoint],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let mut ids = HashSet::new();
    let mut has_mcp = false;
    let mut has_skill = false;
    for server in servers {
        exact_id_string(
            "capability_servers[].id",
            &server.id,
            128,
            "agent_binding_invalid",
        )?;
        if !ids.insert(server.id.as_str()) {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "capability_servers[].id must be unique inside the binding",
                "agent_binding_invalid",
            ));
        }
        validate_registered_endpoint_url(
            "capability_servers[].endpoint_url",
            &server.endpoint_url,
            "agent_binding_invalid",
        )?;
        match server.server_type {
            CapabilityServerType::Mcp => has_mcp = true,
            CapabilityServerType::Skill => has_skill = true,
        }
    }
    if !has_mcp || !has_skill {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "binding v1 requires at least one mcp server and one skill server",
            "agent_binding_invalid",
        ));
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

fn json_string<T: Serialize>(
    value: &T,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    serde_json::to_string(value).map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{column} must be valid JSON"),
            "agent_binding_invalid",
        )
    })
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
    id: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let row = query("SELECT * FROM agent_bindings WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

async fn load_binding_by_idempotency_key(
    pool: &sqlx::Pool<MySql>,
    idempotency_key: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let row = query("SELECT * FROM agent_bindings WHERE idempotency_key = ?")
        .bind(idempotency_key)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

async fn load_binding_by_name(
    pool: &sqlx::Pool<MySql>,
    binding_name: &str,
) -> Result<Option<AgentBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
    let row = query("SELECT * FROM agent_bindings WHERE binding_name = ?")
        .bind(binding_name)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?;
    row.map(agent_binding_from_row).transpose()
}

fn agent_binding_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<AgentBindingRecord, (StatusCode, Json<ErrorResponse>)> {
    let servers_json: String = row
        .try_get("capability_servers_json")
        .map_err(internal_error)?;
    let runtime_policy_json: String = row.try_get("runtime_policy_json").map_err(internal_error)?;
    let metadata_json: Option<String> = row.try_get("metadata_json").ok().flatten();
    Ok(AgentBindingRecord {
        id: row.try_get("id").map_err(internal_error)?,
        binding_name: row.try_get("binding_name").map_err(internal_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(internal_error)?,
        status: AgentBindingStatus::from_db_value(
            &row.try_get::<String, _>("status").map_err(internal_error)?,
        )?,
        agent_md: row.try_get("agent_md").map_err(internal_error)?,
        capability_servers: serde_json::from_str(&servers_json).map_err(internal_error)?,
        runtime_policy: serde_json::from_str(&runtime_policy_json).map_err(internal_error)?,
        metadata: metadata_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(internal_error)?,
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

    fn valid_request() -> AgentBindingCreateRequestData {
        AgentBindingCreateRequestData {
            idempotency_key: "key-01".into(),
            binding: AgentBindingPayload {
                binding_name: "binding-01".into(),
                agent_md: "You are a test agent.".into(),
                capability_servers: vec![
                    CapabilityServerEndpoint {
                        id: "tools".into(),
                        server_type: CapabilityServerType::Mcp,
                        transport: CapabilityServerTransport::StreamableHttp,
                        endpoint_url: "https://capabilities.example.com/mcp".into(),
                    },
                    CapabilityServerEndpoint {
                        id: "skills".into(),
                        server_type: CapabilityServerType::Skill,
                        transport: CapabilityServerTransport::StreamableHttp,
                        endpoint_url: "https://capabilities.example.com/skills".into(),
                    },
                ],
                runtime_policy: RuntimePolicy {
                    max_steps: Some(5),
                    tool_mode: ToolMode::McpGateway,
                },
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
        let first = svc.create_binding(valid_request()).await.unwrap();
        let second = svc.create_binding(valid_request()).await.unwrap();
        assert_eq!(first.id, second.id);
        assert!(first.id.starts_with("ab_"));
    }

    #[tokio::test]
    async fn binding_conflicts_on_same_idempotency_key_different_payload() {
        let svc = InMemoryAgentBindingService::new();
        svc.create_binding(valid_request()).await.unwrap();
        let mut changed = valid_request();
        changed.binding.agent_md = "different".into();
        let err = svc.create_binding(changed).await.unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("agent_binding_idempotency_conflict")
        );
    }

    #[tokio::test]
    async fn binding_rejects_missing_skill_server() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        request
            .binding
            .capability_servers
            .retain(|server| server.server_type != CapabilityServerType::Skill);
        let err = svc.create_binding(request).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }

    #[tokio::test]
    async fn binding_rejects_credential_bearing_endpoint() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        request.binding.capability_servers[0].endpoint_url =
            "https://capabilities.example.com/mcp?token=secret".into();
        let err = svc.create_binding(request).await.unwrap_err();
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
        let err = svc.create_binding(request).await.unwrap_err();
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
        let err = svc.create_binding(request).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }

    #[tokio::test]
    async fn binding_rejects_max_steps_above_server_ceiling() {
        let svc = InMemoryAgentBindingService::new();
        let mut request = valid_request();
        let max_allowed = astra_config::runtime_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(false);
        request.binding.runtime_policy.max_steps = Some((max_allowed as u32).saturating_add(1));

        let err = svc.create_binding(request).await.unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("agent_binding_policy_invalid")
        );
    }
}
