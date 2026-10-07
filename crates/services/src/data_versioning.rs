use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use std::collections::HashSet;

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<String>,
    pub causal_chain_id: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SandboxCheckpointData {
    pub checkpoint_name: String,
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

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    row.try_get(column)
        .map_err(|err| internal_error(format!("invalid {table}.{column}: {err}")))
}

fn required_row_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let value = row_string(row, table, column)?;
    if value.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid {table}.{column}: value is empty"
        )));
    }
    Ok(value)
}

fn optional_row_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let value: Option<String> = row
        .try_get(column)
        .map_err(|err| internal_error(format!("invalid {table}.{column}: {err}")))?;
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(internal_error(format!(
            "invalid {table}.{column}: value is empty"
        )));
    }
    Ok(value)
}

fn checkpoint_response_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
    Ok(CheckpointResponse {
        checkpoint_name: required_row_string(
            &row,
            "data_versioning_checkpoints",
            "checkpoint_name",
        )?,
        timestamp: required_row_string(&row, "data_versioning_checkpoints", "created_at")?,
        description: row.try_get("description").map_err(|err| {
            internal_error(format!(
                "invalid data_versioning_checkpoints.description: {err}"
            ))
        })?,
    })
}

fn event_at_checkpoint_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<EventAtCheckpoint, (StatusCode, Json<ErrorResponse>)> {
    let raw_content = row_string(&row, "agent_events", "content")?;
    Ok(EventAtCheckpoint {
        event_id: required_row_string(&row, "agent_events", "event_id")?,
        session_id: required_row_string(&row, "agent_events", "session_id")?,
        event_type: required_row_string(&row, "agent_events", "event_type")?,
        content: truncate_content(&raw_content, 500),
        created_at: required_row_string(&row, "agent_events", "created_at")?,
    })
}

fn lineage_node_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<LineageNode, (StatusCode, Json<ErrorResponse>)> {
    let event_id = required_row_string(&row, "agent_events", "event_id")?;
    required_row_string(&row, "agent_events", "session_id")?;
    let created_at = required_row_string(&row, "agent_events", "created_at")?;
    let raw_content = row_string(&row, "agent_events", "content")?;
    let parent_event_id = optional_row_string(&row, "agent_events", "parent_event_id")?;
    let causal_chain_id = optional_row_string(&row, "agent_events", "causal_chain_id")?;
    Ok(LineageNode {
        event_id,
        event_type: required_row_string(&row, "agent_events", "event_type")?,
        content: truncate_content(&raw_content, 500),
        parent_event_id,
        parent_event_ids: Vec::new(),
        causal_chain_id,
        created_at,
    })
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseDataVersioningService",
            &self.matrixone,
        )
    }

    async fn hydrate_parent_event_ids(
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
        nodes: &mut [LineageNode],
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let event_ids: Vec<String> = nodes.iter().map(|node| node.event_id.clone()).collect();
        let parent_id_map = crate::storage::load_agent_event_parent_ids(pool, user_id, &event_ids)
            .await
            .map_err(internal_error)?;
        for node in nodes {
            node.parent_event_ids = crate::storage::normalized_parent_event_ids(
                node.parent_event_id.as_deref(),
                parent_id_map.get(&node.event_id).map(Vec::as_slice),
            );
        }
        Ok(())
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

        let checkpoint_id = uuid::Uuid::new_v4().to_string();

        query(
            "INSERT INTO data_versioning_checkpoints \
             (checkpoint_id, checkpoint_name, user_id, description, created_at) \
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(&checkpoint_id)
        .bind(&request.name)
        .bind(&user_id)
        .bind(&request.description)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(
            "SELECT checkpoint_name, description, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM data_versioning_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(&checkpoint_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        checkpoint_response_from_row(row)
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
            checkpoints.push(checkpoint_response_from_row(row)?);
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
        let cp_ts = required_row_string(&cp_row, "data_versioning_checkpoints", "created_at")?;

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
            events.push(event_at_checkpoint_from_row(row)?);
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
        let chain_id = optional_row_string(&seed, "agent_events", "causal_chain_id")?;

        let chain_id = chain_id
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Event has no causal chain"))?;

        let rows = query(
            "SELECT event_id, session_id, event_type, \
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
            nodes.push(lineage_node_from_row(row)?);
        }
        Self::hydrate_parent_event_ids(&pool, &user_id, &mut nodes).await?;
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
        let mut stack = vec![event_id];
        let max_depth = 100;

        while let Some(eid) = stack.pop() {
            if visited.contains(&eid) {
                continue;
            }
            if chain.len() >= max_depth {
                break;
            }
            visited.insert(eid.clone());

            let row = query(
                "SELECT event_id, session_id, event_type, \
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
                    let mut node = lineage_node_from_row(row)?;
                    let parent_id_map = crate::storage::load_agent_event_parent_ids(
                        &pool,
                        &user_id,
                        std::slice::from_ref(&eid),
                    )
                    .await
                    .map_err(internal_error)?;
                    let parent_event_ids = crate::storage::normalized_parent_event_ids(
                        node.parent_event_id.as_deref(),
                        parent_id_map.get(&eid).map(Vec::as_slice),
                    );
                    for parent_event_id in parent_event_ids.iter().rev() {
                        if !visited.contains(parent_event_id) {
                            stack.push(parent_event_id.clone());
                        }
                    }
                    node.parent_event_ids = parent_event_ids;
                    chain.push(node);
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

        let checkpoint_id = uuid::Uuid::new_v4().to_string();

        query(
            "INSERT INTO data_versioning_checkpoints \
             (checkpoint_id, checkpoint_name, user_id, description, created_at) \
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(&checkpoint_id)
        .bind(&full_name)
        .bind(&user_id)
        .bind(format!("Sandbox checkpoint for {}", sandbox_name))
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(
            "SELECT checkpoint_name, description, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM data_versioning_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(&checkpoint_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        checkpoint_response_from_row(row)
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

    // ── validate_checkpoint_name (5→1 data-driven) ──

    #[test]
    fn test_validate_checkpoint_name() {
        let name_128 = "x".repeat(128);
        let name_129 = "x".repeat(129);
        let cases: Vec<(&str, bool)> = vec![
            ("my-checkpoint", true),
            ("cp_123", true),
            ("a", true),
            (&name_128, true),
            ("", false),
            (&name_129, false),
            ("my checkpoint", false),
            ("cp/bad", false),
            ("cp.dot", false),
            ("检查点", false),
        ];
        for (name, expect_ok) in cases {
            assert_eq!(
                validate_checkpoint_name(name).is_ok(),
                expect_ok,
                "name={:?} expect_ok={}",
                name,
                expect_ok
            );
        }
    }

    // ── truncate_content (4→1 data-driven) ──

    #[test]
    fn truncate_content() {
        let cases = vec![
            ("hello", 10, "hello"),
            ("hello", 5, "hello"),
            ("hello world", 5, "hello..."),
            ("", 0, ""),
            ("", 100, ""),
        ];
        for (input, max_len, expect) in cases {
            assert_eq!(super::truncate_content(input, max_len), expect);
        }
    }

    // ── Serialization (2→1 data-driven) ──

    #[test]
    fn checkpoint_response_serialization() {
        let none_desc = CheckpointResponse {
            checkpoint_name: "cp1".into(),
            timestamp: "2024-01-01T00:00:00".into(),
            description: None,
        };
        let json = serde_json::to_string(&none_desc).unwrap();
        assert!(!json.contains("description"));

        let with_desc = CheckpointResponse {
            checkpoint_name: "cp1".into(),
            timestamp: "2024-01-01T00:00:00".into(),
            description: Some("test".into()),
        };
        let json = serde_json::to_string(&with_desc).unwrap();
        assert!(json.contains("\"description\":\"test\""));
    }

    #[test]
    fn lineage_node_serialization_roundtrip() {
        let node = LineageNode {
            event_id: "e1".into(),
            event_type: "tool_call_completed".into(),
            content: "hello".into(),
            parent_event_id: Some("e0".into()),
            parent_event_ids: vec!["e0".into(), "e2".into()],
            causal_chain_id: None,
            created_at: "2024-01-01T00:00:00".into(),
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"parent_event_id\":\"e0\""));
        assert!(json.contains("\"parent_event_ids\":[\"e0\",\"e2\"]"));
        assert!(!json.contains("contribution_score"));
        assert!(json.contains("\"causal_chain_id\":null"));
    }

    #[test]
    fn normalized_parent_event_ids_keep_primary_first() {
        let normalized = crate::storage::normalized_parent_event_ids(
            Some("p0"),
            Some(&["p0".to_string(), "p2".to_string(), "p1".to_string()]),
        );
        assert_eq!(normalized, vec!["p0", "p2", "p1"]);
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
    }

    // ── Data type equality (2→1) ──

    #[test]
    fn checkpoint_data_equality() {
        let a = CreateCheckpointData {
            name: "cp1".into(),
            description: Some("d".into()),
        };
        let b = CreateCheckpointData {
            name: "cp1".into(),
            description: Some("d".into()),
        };
        assert_eq!(a, b);

        let sandbox_a = SandboxCheckpointData {
            checkpoint_name: "snap1".into(),
        };
        let sandbox_b = sandbox_a.clone();
        assert_eq!(sandbox_a, sandbox_b);
    }
}
