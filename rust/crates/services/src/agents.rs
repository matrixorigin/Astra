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
pub struct AgentListItem {
    pub agent_id: String,
    pub name: String,
    pub agent_type: String,
    pub owner_user_id: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentListRecord {
    pub agents: Vec<AgentListItem>,
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

const AGENT_LIST_SELECT_COLS: &str = "\
    agent_id, agent_name, agent_type, owner_user_id, is_active, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at";
const MAX_AGENT_LIST_ROWS: i64 = 200;

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
            "SELECT {} FROM agent_agents WHERE owner_user_id = ? ORDER BY created_at DESC LIMIT ?",
            AGENT_LIST_SELECT_COLS
        );
        let rows = query(&select_sql)
            .bind(&user_id)
            .bind(MAX_AGENT_LIST_ROWS)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            let is_active_int: i16 = row.try_get("is_active").unwrap_or(1);
            agents.push(AgentListItem {
                agent_id: row.try_get("agent_id").map_err(internal_error)?,
                name: row.try_get("agent_name").map_err(internal_error)?,
                agent_type: row.try_get("agent_type").map_err(internal_error)?,
                owner_user_id: row.try_get("owner_user_id").map_err(internal_error)?,
                is_active: is_active_int != 0,
                created_at: row.try_get("created_at").unwrap_or_default(),
                updated_at: row.try_get("updated_at").ok(),
            });
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

// ── In-memory implementation for testing ─────────────────────────────────────

pub struct InMemoryAgentService {
    agents: std::sync::RwLock<Vec<AgentRecord>>,
}

impl Default for InMemoryAgentService {
    fn default() -> Self { Self::new() }
}

impl InMemoryAgentService {
    pub fn new() -> Self {
        Self { agents: std::sync::RwLock::new(Vec::new()) }
    }
}

#[async_trait]
impl AgentService for InMemoryAgentService {
    async fn create_agent(
        &self,
        user_id: String,
        request: AgentCreateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let now = chrono::Utc::now().to_rfc3339();
        let record = AgentRecord {
            agent_id: Uuid::new_v4().to_string(),
            name: request.name,
            agent_type: "custom".to_string(),
            owner_user_id: user_id,
            agent_config: request.agent_config.unwrap_or(serde_json::json!({})),
            data_source: request.data_source.unwrap_or(serde_json::json!({})),
            is_active: true,
            created_at: now,
            updated_at: None,
        };
        self.agents.write().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_agents(
        &self,
        user_id: String,
    ) -> Result<AgentListRecord, (StatusCode, Json<ErrorResponse>)> {
        let agents = self.agents.read().unwrap();
        let items: Vec<AgentListItem> = agents.iter()
            .filter(|a| a.owner_user_id == user_id)
            .map(|a| AgentListItem {
                agent_id: a.agent_id.clone(),
                name: a.name.clone(),
                agent_type: a.agent_type.clone(),
                owner_user_id: a.owner_user_id.clone(),
                is_active: a.is_active,
                created_at: a.created_at.clone(),
                updated_at: a.updated_at.clone(),
            })
            .collect();
        let total = items.len() as i64;
        Ok(AgentListRecord { agents: items, total })
    }

    async fn get_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let agents = self.agents.read().unwrap();
        agents.iter()
            .find(|a| a.agent_id == agent_id && a.owner_user_id == user_id)
            .cloned()
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "agent not found"))
    }

    async fn update_agent(
        &self,
        agent_id: String,
        user_id: String,
        request: AgentUpdateRequestData,
    ) -> Result<AgentRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut agents = self.agents.write().unwrap();
        let agent = agents.iter_mut()
            .find(|a| a.agent_id == agent_id && a.owner_user_id == user_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "agent not found"))?;
        if let Some(name) = request.name { agent.name = name; }
        if let Some(config) = request.agent_config { agent.agent_config = config; }
        if let Some(ds) = request.data_source { agent.data_source = ds; }
        if let Some(active) = request.is_active { agent.is_active = active; }
        agent.updated_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(agent.clone())
    }

    async fn delete_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let mut agents = self.agents.write().unwrap();
        let len_before = agents.len();
        agents.retain(|a| !(a.agent_id == agent_id && a.owner_user_id == user_id));
        if agents.len() == len_before {
            Err(error_response(StatusCode::NOT_FOUND, "agent not found"))
        } else {
            Ok(())
        }
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
    pub agents: Vec<AgentListItemResponse>,
    pub total: i64,
}

#[derive(Serialize, PartialEq)]
pub struct AgentListItemResponse {
    pub agent_id: String,
    pub name: String,
    pub agent_type: String,
    pub owner_user_id: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
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
            agents: r
                .agents
                .into_iter()
                .map(|agent| AgentListItemResponse {
                    agent_id: agent.agent_id,
                    name: agent.name,
                    agent_type: agent.agent_type,
                    owner_user_id: agent.owner_user_id,
                    is_active: agent.is_active,
                    created_at: agent.created_at,
                    updated_at: agent.updated_at,
                })
                .collect(),
            total: r.total,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_agent_returns_record() {
        let svc = InMemoryAgentService::new();
        let record = svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "my-agent".into(),
            agent_config: Some(serde_json::json!({"model": "gpt-4"})),
            data_source: None,
        }).await.unwrap();
        assert_eq!(record.name, "my-agent");
        assert_eq!(record.owner_user_id, "u1");
        assert!(record.is_active);
        assert!(!record.agent_id.is_empty());
    }

    #[tokio::test]
    async fn list_agents_filters_by_user() {
        let svc = InMemoryAgentService::new();
        svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "a1".into(), agent_config: None, data_source: None,
        }).await.unwrap();
        svc.create_agent("u2".into(), AgentCreateRequestData {
            name: "a2".into(), agent_config: None, data_source: None,
        }).await.unwrap();

        let list = svc.list_agents("u1".into()).await.unwrap();
        assert_eq!(list.total, 1);
        assert_eq!(list.agents[0].name, "a1");
    }

    #[tokio::test]
    async fn get_agent_by_id() {
        let svc = InMemoryAgentService::new();
        let created = svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "test".into(), agent_config: None, data_source: None,
        }).await.unwrap();

        let fetched = svc.get_agent(created.agent_id.clone(), "u1".into()).await.unwrap();
        assert_eq!(fetched.agent_id, created.agent_id);
    }

    #[tokio::test]
    async fn get_nonexistent_agent_returns_404() {
        let svc = InMemoryAgentService::new();
        let result = svc.get_agent("nope".into(), "u1".into()).await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_agent_fields() {
        let svc = InMemoryAgentService::new();
        let created = svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "old".into(), agent_config: None, data_source: None,
        }).await.unwrap();

        let updated = svc.update_agent(created.agent_id.clone(), "u1".into(), AgentUpdateRequestData {
            name: Some("new".into()),
            agent_config: None,
            data_source: None,
            is_active: Some(false),
        }).await.unwrap();
        assert_eq!(updated.name, "new");
        assert!(!updated.is_active);
        assert!(updated.updated_at.is_some());
    }

    #[tokio::test]
    async fn delete_agent_removes_it() {
        let svc = InMemoryAgentService::new();
        let created = svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "doomed".into(), agent_config: None, data_source: None,
        }).await.unwrap();

        svc.delete_agent(created.agent_id.clone(), "u1".into()).await.unwrap();
        let result = svc.get_agent(created.agent_id, "u1".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_404() {
        let svc = InMemoryAgentService::new();
        let result = svc.delete_agent("nope".into(), "u1".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn user_isolation_on_get() {
        let svc = InMemoryAgentService::new();
        let created = svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "private".into(), agent_config: None, data_source: None,
        }).await.unwrap();

        // u2 cannot access u1's agent
        let result = svc.get_agent(created.agent_id, "u2".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unconfigured_service_returns_error() {
        let svc = UnconfiguredAgentService;
        assert!(svc.list_agents("u1".into()).await.is_err());
        assert!(svc.create_agent("u1".into(), AgentCreateRequestData {
            name: "x".into(), agent_config: None, data_source: None,
        }).await.is_err());
    }

    #[test]
    fn agent_response_from_record() {
        let record = AgentRecord {
            agent_id: "a1".into(),
            name: "test".into(),
            agent_type: "custom".into(),
            owner_user_id: "u1".into(),
            agent_config: serde_json::json!({}),
            data_source: serde_json::json!({}),
            is_active: true,
            created_at: "2026-01-01".into(),
            updated_at: None,
        };
        let resp = AgentResponse::from(record);
        assert_eq!(resp.agent_id, "a1");
        assert_eq!(resp.name, "test");
    }

    #[test]
    fn agent_list_response_from_record() {
        let record = AgentListRecord {
            agents: vec![AgentListItem {
                agent_id: "a1".into(),
                name: "test".into(),
                agent_type: "custom".into(),
                owner_user_id: "u1".into(),
                is_active: true,
                created_at: "2026-01-01".into(),
                updated_at: None,
            }],
            total: 1,
        };
        let resp = AgentListResponse::from(record);
        assert_eq!(resp.total, 1);
        assert_eq!(resp.agents[0].agent_id, "a1");
    }
}
