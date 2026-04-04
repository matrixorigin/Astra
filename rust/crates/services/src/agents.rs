use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AgentCreateRequestData {
    pub name: String,
    pub agent_config: Option<serde_json::Value>,
    pub data_source: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentUpdateRequestData {
    pub name: Option<String>,
    pub agent_config: Option<serde_json::Value>,
    pub data_source: Option<serde_json::Value>,
    pub is_active: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRecord {
    pub agent_id: String,
    pub name: String,
    pub agent_type: String,
    pub owner_user_id: String,
    pub agent_config: serde_json::Value,
    pub data_source: serde_json::Value,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentListRecord {
    pub agents: Vec<AgentRecord>,
    pub total: i64,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait AgentService: Send + Sync {
    async fn create_agent(
        &self,
        user_id: String,
        request: AgentCreateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_agents(
        &self,
        user_id: String,
    ) -> Result<AgentListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn update_agent(
        &self,
        agent_id: String,
        user_id: String,
        request: AgentUpdateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseAgentService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAgentService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn agent_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let config_json: String = row
            .try_get("agent_config_json")
            .unwrap_or_else(|_| "{}".to_string());
        let source_json: String = row
            .try_get("data_source_json")
            .unwrap_or_else(|_| "{}".to_string());
        let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);

        Ok(AgentRecord {
            agent_id: row.try_get("agent_id").map_err(internal_error)?,
            name: row.try_get("agent_name").map_err(internal_error)?,
            agent_type: row.try_get("agent_type").map_err(internal_error)?,
            owner_user_id: row.try_get("owner_user_id").map_err(internal_error)?,
            agent_config: serde_json::from_str(&config_json)
                .unwrap_or(serde_json::Value::Object(Default::default())),
            data_source: serde_json::from_str(&source_json)
                .unwrap_or(serde_json::Value::Object(Default::default())),
            is_active: is_active_int != 0,
            created_at: row.try_get("created_at").unwrap_or_default(),
            updated_at: row.try_get("updated_at").ok(),
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

pub const AGENT_SELECT_COLS: &str = "\
    agent_id, agent_name, agent_type, owner_user_id, is_active, \
    IFNULL(CAST(agent_config AS CHAR), '{}') AS agent_config_json, \
    IFNULL(CAST(data_source AS CHAR), '{}') AS data_source_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at";

#[async_trait]
impl AgentService for DatabaseAgentService {
    async fn create_agent(
        &self,
        user_id: String,
        request: AgentCreateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let agent_id = Uuid::new_v4().to_string();
        let config_str = request
            .agent_config
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string());
        let source_str = request
            .data_source
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| r#"{"type":"matrixone","database":"astra_runtime"}"#.to_string());

        query(
            "INSERT INTO agent_agents \
             (agent_id, agent_name, agent_type, owner_user_id, is_active, agent_config, data_source, created_at, updated_at) \
             VALUES (?, ?, 'general', ?, 1, ?, ?, NOW(), NOW())",
        )
        .bind(&agent_id)
        .bind(&request.name)
        .bind(&user_id)
        .bind(&config_str)
        .bind(&source_str)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_agents WHERE agent_id = ?",
            AGENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&agent_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        Self::agent_record_from_row(row)
    }

    async fn list_agents(
        &self,
        user_id: String,
    ) -> Result<AgentListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let count_row =
            query("SELECT COUNT(agent_id) AS total FROM agent_agents WHERE owner_user_id = ?")
                .bind(&user_id)
                .fetch_one(&pool)
                .await
                .map_err(internal_error)?;
        let total = count_row.try_get::<i64, _>("total").unwrap_or(0);

        let select_sql = format!(
            "SELECT {} FROM agent_agents WHERE owner_user_id = ? ORDER BY created_at DESC",
            AGENT_SELECT_COLS
        );
        let rows = query(&select_sql)
            .bind(&user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            agents.push(Self::agent_record_from_row(row)?);
        }

        Ok(AgentListRecord { agents, total })
    }

    async fn get_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_agents WHERE agent_id = ?",
            AGENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&agent_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            )
        })?;
        let record = Self::agent_record_from_row(row)?;
        if record.owner_user_id != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            ));
        }
        Ok(record)
    }

    async fn update_agent(
        &self,
        agent_id: String,
        user_id: String,
        request: AgentUpdateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let check_row = query("SELECT owner_user_id FROM agent_agents WHERE agent_id = ?")
            .bind(&agent_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let check_row = check_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            )
        })?;
        let owner: String = check_row.try_get("owner_user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            ));
        }

        let mut builder = QueryBuilder::<MySql>::new("UPDATE agent_agents SET updated_at = NOW()");
        if let Some(name) = &request.name {
            builder.push(", agent_name = ");
            builder.push_bind(name);
        }
        if let Some(config) = &request.agent_config {
            builder.push(", agent_config = ");
            builder.push_bind(config.to_string());
        }
        if let Some(source) = &request.data_source {
            builder.push(", data_source = ");
            builder.push_bind(source.to_string());
        }
        if let Some(active) = request.is_active {
            builder.push(", is_active = ");
            builder.push_bind(if active { 1i16 } else { 0i16 });
        }
        builder.push(" WHERE agent_id = ");
        builder.push_bind(&agent_id);

        builder
            .build()
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_agents WHERE agent_id = ?",
            AGENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&agent_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        Self::agent_record_from_row(row)
    }

    async fn delete_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let check_row = query("SELECT owner_user_id FROM agent_agents WHERE agent_id = ?")
            .bind(&agent_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let check_row = check_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            )
        })?;
        let owner: String = check_row.try_get("owner_user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Agent {} not found", agent_id),
            ));
        }

        query("DELETE FROM agent_agents WHERE agent_id = ? AND owner_user_id = ?")
            .bind(&agent_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(())
    }
}

// ── Noop implementation for tests ────────────────────────────────────────────

pub struct UnconfiguredAgentService;

#[async_trait]
impl AgentService for UnconfiguredAgentService {
    async fn create_agent(
        &self,
        _user_id: String,
        _request: AgentCreateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("agent service not configured"))
    }
    async fn list_agents(
        &self,
        _user_id: String,
    ) -> Result<AgentListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("agent service not configured"))
    }
    async fn get_agent(
        &self,
        _agent_id: String,
        _user_id: String,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("agent service not configured"))
    }
    async fn update_agent(
        &self,
        _agent_id: String,
        _user_id: String,
        _request: AgentUpdateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("agent service not configured"))
    }
    async fn delete_agent(
        &self,
        _agent_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("agent service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AgentCreateRequest {
    pub name: String,
    pub agent_config: Option<serde_json::Value>,
    pub data_source: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct AgentUpdateRequest {
    pub name: Option<String>,
    pub agent_config: Option<serde_json::Value>,
    pub data_source: Option<serde_json::Value>,
    pub is_active: Option<bool>,
}

#[derive(Serialize, PartialEq)]
pub struct AgentResponse {
    pub agent_id: String,
    pub name: String,
    pub agent_type: String,
    pub owner_user_id: String,
    pub agent_config: serde_json::Value,
    pub data_source: serde_json::Value,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

#[derive(Serialize, PartialEq)]
pub struct AgentListResponse {
    pub agents: Vec<AgentResponse>,
    pub total: i64,
}

impl From<AgentRecord> for AgentResponse {
    fn from(r: AgentRecord) -> Self {
        Self {
            agent_id: r.agent_id,
            name: r.name,
            agent_type: r.agent_type,
            owner_user_id: r.owner_user_id,
            agent_config: r.agent_config,
            data_source: r.data_source,
            is_active: r.is_active,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

impl From<AgentListRecord> for AgentListResponse {
    fn from(r: AgentListRecord) -> Self {
        Self {
            agents: r.agents.into_iter().map(AgentResponse::from).collect(),
            total: r.total,
        }
    }
}
