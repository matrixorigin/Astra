use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use std::collections::HashSet;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

const MAX_CHECKPOINT_LIST_ROWS: i32 = 200;
const MAX_CHECKPOINT_EVENT_ROWS: i32 = 200;
const MAX_CAUSAL_CHAIN_ROWS: i32 = 500;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct CreateCheckpointData {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckpointResponse {
    pub checkpoint_name: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventAtCheckpoint {
    pub event_id: String,
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LineageNode {
    pub event_id: String,
    pub event_type: String,
    pub content: String,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SandboxCheckpointData {
    pub checkpoint_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StatusResponse {
    pub status: String,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn validate_checkpoint_name(name: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if name.is_empty() || name.len() > 128 {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Checkpoint name must be 1-128 characters",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Checkpoint name must contain only alphanumeric, underscore, or hyphen characters",
        ));
    }
    Ok(())
}

pub fn truncate_content(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait DataVersioningService: Send + Sync {
    async fn create_checkpoint(
        &self,
        user_id: String,
        request: CreateCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn list_checkpoints(
        &self,
        user_id: String,
    ) -> Result<Vec<CheckpointResponse>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_events_at_checkpoint(
        &self,
        user_id: String,
        checkpoint_name: String,
    ) -> Result<Vec<EventAtCheckpoint>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_causal_chain(
        &self,
        user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)>;

    async fn trace_upstream(
        &self,
        user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)>;

    async fn sandbox_checkpoint(
        &self,
        user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn sandbox_restore(
        &self,
        user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseDataVersioningService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseDataVersioningService {
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

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl DataVersioningService for DatabaseDataVersioningService {
    async fn create_checkpoint(
        &self,
        user_id: String,
        request: CreateCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_checkpoint_name(&request.name)?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = crate::snapshot_sql::create_snapshot_for_db_sql(
            &request.name,
            &self.matrixone.database,
        );
        query(&sql).execute(&pool).await.map_err(internal_error)?;

        query(
            "INSERT INTO data_versioning_checkpoints \
             (checkpoint_name, user_id, description, created_at) \
             VALUES (?, ?, ?, NOW())",
        )
        .bind(&request.name)
        .bind(&user_id)
        .bind(&request.description)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(CheckpointResponse {
            checkpoint_name: request.name,
            timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            description: request.description,
        })
    }

    async fn list_checkpoints(
        &self,
        user_id: String,
    ) -> Result<Vec<CheckpointResponse>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = query(
            "SELECT checkpoint_name, description, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
              FROM data_versioning_checkpoints \
              WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&user_id)
        .bind(MAX_CHECKPOINT_LIST_ROWS)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut checkpoints = Vec::with_capacity(rows.len());
        for row in rows {
            checkpoints.push(CheckpointResponse {
                checkpoint_name: row.try_get("checkpoint_name").map_err(internal_error)?,
                timestamp: row.try_get("created_at").unwrap_or_default(),
                description: row.try_get("description").ok(),
            });
        }
        Ok(checkpoints)
    }

    async fn get_events_at_checkpoint(
        &self,
        user_id: String,
        checkpoint_name: String,
    ) -> Result<Vec<EventAtCheckpoint>, (StatusCode, Json<ErrorResponse>)> {
        validate_checkpoint_name(&checkpoint_name)?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let cp_row = query(
            "SELECT DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM data_versioning_checkpoints \
             WHERE checkpoint_name = ? AND user_id = ?",
        )
        .bind(&checkpoint_name)
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let cp_row = cp_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Checkpoint '{}' not found", checkpoint_name),
            )
        })?;
        let cp_ts: String = cp_row.try_get("created_at").unwrap_or_default();

        let rows = query(
            "SELECT event_id, session_id, event_type, \
             SUBSTRING(IFNULL(CAST(content AS CHAR), ''), 1, 500) AS content, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
              FROM agent_events \
              WHERE user_id = ? AND created_at <= ? \
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&user_id)
        .bind(&cp_ts)
        .bind(MAX_CHECKPOINT_EVENT_ROWS)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_content: String = row.try_get("content").unwrap_or_default();
            events.push(EventAtCheckpoint {
                event_id: row.try_get("event_id").map_err(internal_error)?,
                session_id: row.try_get("session_id").map_err(internal_error)?,
                event_type: row.try_get("event_type").map_err(internal_error)?,
                content: truncate_content(&raw_content, 500),
                created_at: row.try_get("created_at").unwrap_or_default(),
            });
        }
        Ok(events)
    }

    async fn get_causal_chain(
        &self,
        user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let seed =
            query("SELECT causal_chain_id FROM agent_events WHERE event_id = ? AND user_id = ?")
                .bind(&event_id)
                .bind(&user_id)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;

        let seed = seed.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Event '{}' not found", event_id),
            )
        })?;
        let chain_id: Option<String> = seed.try_get("causal_chain_id").ok();

        let chain_id = chain_id
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Event has no causal chain"))?;

        let rows = query(
            "SELECT event_id, event_type, \
             SUBSTRING(IFNULL(CAST(content AS CHAR), ''), 1, 500) AS content, \
             parent_event_id, causal_chain_id, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
              FROM agent_events \
              WHERE user_id = ? AND causal_chain_id = ? \
              ORDER BY created_at ASC LIMIT ?",
        )
        .bind(&user_id)
        .bind(&chain_id)
        .bind(MAX_CAUSAL_CHAIN_ROWS)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_content: String = row.try_get("content").unwrap_or_default();
            nodes.push(LineageNode {
                event_id: row.try_get("event_id").map_err(internal_error)?,
                event_type: row.try_get("event_type").map_err(internal_error)?,
                content: truncate_content(&raw_content, 500),
                parent_event_id: row.try_get("parent_event_id").ok(),
                causal_chain_id: row.try_get("causal_chain_id").ok(),
                created_at: row.try_get("created_at").unwrap_or_default(),
            });
        }
        Ok(nodes)
    }

    async fn trace_upstream(
        &self,
        user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut current_id = Some(event_id);
        let max_depth = 100;

        while let Some(eid) = current_id.take() {
            if visited.contains(&eid) || chain.len() >= max_depth {
                break;
            }
            visited.insert(eid.clone());

            let row = query(
                "SELECT event_id, event_type, \
                 SUBSTRING(IFNULL(CAST(content AS CHAR), ''), 1, 500) AS content, \
                 parent_event_id, causal_chain_id, \
                 DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events WHERE event_id = ? AND user_id = ?",
            )
            .bind(&eid)
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

            match row {
                Some(row) => {
                    let raw_content: String = row.try_get("content").unwrap_or_default();
                    let parent: Option<String> = row.try_get("parent_event_id").ok();
                    current_id = parent.clone();
                    chain.push(LineageNode {
                        event_id: row.try_get("event_id").map_err(internal_error)?,
                        event_type: row.try_get("event_type").map_err(internal_error)?,
                        content: truncate_content(&raw_content, 500),
                        parent_event_id: parent,
                        causal_chain_id: row.try_get("causal_chain_id").ok(),
                        created_at: row.try_get("created_at").unwrap_or_default(),
                    });
                }
                None => break,
            }
        }

        Ok(chain)
    }

    async fn sandbox_checkpoint(
        &self,
        user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_checkpoint_name(&request.checkpoint_name)?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let full_name = format!("{}__{}", sandbox_name, request.checkpoint_name);
        let sql =
            crate::snapshot_sql::create_snapshot_for_db_sql(&full_name, &self.matrixone.database);
        query(&sql).execute(&pool).await.map_err(internal_error)?;

        query(
            "INSERT INTO data_versioning_checkpoints \
             (checkpoint_name, user_id, description, created_at) \
             VALUES (?, ?, ?, NOW())",
        )
        .bind(&full_name)
        .bind(&user_id)
        .bind(format!("Sandbox checkpoint for {}", sandbox_name))
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(CheckpointResponse {
            checkpoint_name: full_name,
            timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            description: Some(format!("Sandbox checkpoint for {}", sandbox_name)),
        })
    }

    async fn sandbox_restore(
        &self,
        _user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        validate_checkpoint_name(&request.checkpoint_name)?;

        let pool = self.get_pool().await.map_err(internal_error)?;

        let full_name = format!("{}__{}", sandbox_name, request.checkpoint_name);
        let account = crate::snapshot_sql::resolve_account_name(&pool)
            .await
            .map_err(internal_error)?;
        let sql = crate::snapshot_sql::restore_snapshot_db_sql(
            &full_name,
            &account,
            &self.matrixone.database,
        );
        query(&sql).execute(&pool).await.map_err(internal_error)?;

        Ok(StatusResponse {
            status: "restored".into(),
        })
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredDataVersioningService;

#[async_trait]
impl DataVersioningService for UnconfiguredDataVersioningService {
    async fn create_checkpoint(
        &self,
        _: String,
        _: CreateCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn list_checkpoints(
        &self,
        _: String,
    ) -> Result<Vec<CheckpointResponse>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn get_events_at_checkpoint(
        &self,
        _: String,
        _: String,
    ) -> Result<Vec<EventAtCheckpoint>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn get_causal_chain(
        &self,
        _: String,
        _: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn trace_upstream(
        &self,
        _: String,
        _: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn sandbox_checkpoint(
        &self,
        _: String,
        _: String,
        _: SandboxCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
    async fn sandbox_restore(
        &self,
        _: String,
        _: String,
        _: SandboxCheckpointData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("data versioning service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateCheckpointRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct SandboxCheckpointRequest {
    pub checkpoint_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_checkpoint_name ──

    #[test]
    fn validate_checkpoint_name_valid_names() {
        assert!(validate_checkpoint_name("my-checkpoint").is_ok());
        assert!(validate_checkpoint_name("cp_123").is_ok());
        assert!(validate_checkpoint_name("a").is_ok());
        assert!(validate_checkpoint_name(&"x".repeat(128)).is_ok());
    }

    #[test]
    fn validate_checkpoint_name_empty() {
        let err = validate_checkpoint_name("");
        assert!(err.is_err());
    }

    #[test]
    fn validate_checkpoint_name_too_long() {
        assert!(validate_checkpoint_name(&"x".repeat(129)).is_err());
    }

    #[test]
    fn validate_checkpoint_name_special_chars() {
        assert!(validate_checkpoint_name("my checkpoint").is_err());
        assert!(validate_checkpoint_name("cp/bad").is_err());
        assert!(validate_checkpoint_name("cp.dot").is_err());
    }

    #[test]
    fn validate_checkpoint_name_unicode() {
        assert!(validate_checkpoint_name("检查点").is_err());
    }

    // ── truncate_content ──

    #[test]
    fn truncate_content_short_unchanged() {
        assert_eq!(truncate_content("hello", 10), "hello");
    }

    #[test]
    fn truncate_content_exact_length() {
        assert_eq!(truncate_content("hello", 5), "hello");
    }

    #[test]
    fn truncate_content_long_adds_ellipsis() {
        assert_eq!(truncate_content("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_content_empty() {
        assert_eq!(truncate_content("", 0), "");
        assert_eq!(truncate_content("", 100), "");
    }

    // ── Serialization ──

    #[test]
    fn checkpoint_response_skip_serializing_none_description() {
        let r = CheckpointResponse {
            checkpoint_name: "cp1".into(),
            timestamp: "2024-01-01T00:00:00".into(),
            description: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("description"));
    }

    #[test]
    fn checkpoint_response_includes_description() {
        let r = CheckpointResponse {
            checkpoint_name: "cp1".into(),
            timestamp: "2024-01-01T00:00:00".into(),
            description: Some("test".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"description\":\"test\""));
    }

    #[test]
    fn lineage_node_serialization_roundtrip() {
        let node = LineageNode {
            event_id: "e1".into(),
            event_type: "tool_call".into(),
            content: "hello".into(),
            parent_event_id: Some("e0".into()),
            causal_chain_id: None,
            created_at: "2024-01-01T00:00:00".into(),
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"parent_event_id\":\"e0\""));
        assert!(json.contains("\"causal_chain_id\":null"));
    }

    #[test]
    fn event_at_checkpoint_serialization() {
        let e = EventAtCheckpoint {
            event_id: "e1".into(),
            session_id: "s1".into(),
            event_type: "user_message".into(),
            content: "test".into(),
            created_at: "2024-01-01".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event_id"], "e1");
        assert_eq!(parsed["session_id"], "s1");
    }

    #[test]
    fn status_response_serialization() {
        let r = StatusResponse {
            status: "restored".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"status":"restored"}"#);
    }

    // ── Request deserialization ──

    #[test]
    fn create_checkpoint_request_deserialize() {
        let json = r#"{"name":"cp1","description":"test"}"#;
        let r: CreateCheckpointRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.name, "cp1");
        assert_eq!(r.description.as_deref(), Some("test"));
    }

    #[test]
    fn create_checkpoint_request_no_description() {
        let json = r#"{"name":"cp1"}"#;
        let r: CreateCheckpointRequest = serde_json::from_str(json).unwrap();
        assert!(r.description.is_none());
    }

    #[test]
    fn sandbox_checkpoint_request_deserialize() {
        let json = r#"{"checkpoint_name":"snap1"}"#;
        let r: SandboxCheckpointRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.checkpoint_name, "snap1");
    }

    // ── UnconfiguredDataVersioningService ──

    #[tokio::test]
    async fn unconfigured_service_returns_errors() {
        let svc = UnconfiguredDataVersioningService;
        assert!(
            svc.create_checkpoint(
                "u1".into(),
                CreateCheckpointData {
                    name: "cp".into(),
                    description: None
                }
            )
            .await
            .is_err()
        );
        assert!(svc.list_checkpoints("u1".into()).await.is_err());
        assert!(
            svc.get_events_at_checkpoint("u1".into(), "cp".into())
                .await
                .is_err()
        );
        assert!(
            svc.get_causal_chain("u1".into(), "e1".into())
                .await
                .is_err()
        );
        assert!(svc.trace_upstream("u1".into(), "e1".into()).await.is_err());
        assert!(
            svc.sandbox_checkpoint(
                "u1".into(),
                "sb".into(),
                SandboxCheckpointData {
                    checkpoint_name: "cp".into()
                }
            )
            .await
            .is_err()
        );
        assert!(
            svc.sandbox_restore(
                "u1".into(),
                "sb".into(),
                SandboxCheckpointData {
                    checkpoint_name: "cp".into()
                }
            )
            .await
            .is_err()
        );
    }

    // ── Data type equality ──

    #[test]
    fn create_checkpoint_data_equality() {
        let a = CreateCheckpointData {
            name: "cp1".into(),
            description: Some("d".into()),
        };
        let b = CreateCheckpointData {
            name: "cp1".into(),
            description: Some("d".into()),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn sandbox_checkpoint_data_equality() {
        let a = SandboxCheckpointData {
            checkpoint_name: "snap1".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
