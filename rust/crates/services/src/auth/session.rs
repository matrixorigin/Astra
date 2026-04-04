use crate::storage::{log_session_audit, session_record_from_row};
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::Utc;
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

#[async_trait]
pub trait SessionService: Send + Sync {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_sessions(
        &self,
        filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn get_session_activity(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionCreateRequestData {
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionUpdateRequestData {
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionListFilter {
    pub user_id: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub status: String,
    pub event_count: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionListRecord {
    pub sessions: Vec<SessionRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActivityEntryRecord {
    pub log_id: String,
    pub action: String,
    pub details: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActivityRecord {
    pub session_id: String,
    pub activities: Vec<SessionActivityEntryRecord>,
    pub total: i64,
}

#[derive(Clone, Debug)]
pub struct DatabaseSessionService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSessionService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    async fn fetch_session_by_id(
        &self,
        pool: &sqlx::Pool<MySql>,
        session_id: &str,
    ) -> Result<Option<SessionRecord>, (StatusCode, Json<ErrorResponse>)> {
        query(
            "SELECT session_id, user_id, agent_id, title, status, event_count, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at, \
             DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s') AS ended_at, \
             IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json \
             FROM agent_sessions WHERE session_id = ? LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?
        .map(session_record_from_row)
        .transpose()
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
impl SessionService for DatabaseSessionService {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let session_id = Uuid::new_v4().to_string();
        let title = request
            .title
            .unwrap_or_else(|| format!("Session {}", Utc::now().format("%Y-%m-%d %H:%M")));
        let metadata = serde_json::Value::Object(request.metadata.unwrap_or_default()).to_string();

        query(
            "INSERT INTO agent_sessions \
             (session_id, user_id, agent_id, title, status, event_count, created_at, updated_at, last_active_at, `metadata`) \
             VALUES (?, ?, ?, ?, 'active', 0, NOW(), NOW(), NOW(), ?)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&request.agent_id)
        .bind(&title)
        .bind(metadata)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let record = self
            .fetch_session_by_id(&pool, &session_id)
            .await?
            .ok_or_else(|| internal_error("failed to read created session"))?;
        let details = serde_json::json!({
            "title": record.title,
            "agent_id": record.agent_id,
        });
        log_session_audit(&pool, &user_id, "session_create", &session_id, details).await;
        Ok(record)
    }

    async fn list_sessions(
        &self,
        filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut count_query = QueryBuilder::<MySql>::new(
            "SELECT COUNT(session_id) AS total FROM agent_sessions WHERE user_id = ",
        );
        count_query.push_bind(&filter.user_id);
        if let Some(agent_id) = &filter.agent_id {
            count_query.push(" AND agent_id = ");
            count_query.push_bind(agent_id);
        }
        if let Some(status) = &filter.status {
            count_query.push(" AND status = ");
            count_query.push_bind(status);
        }

        let total_row = count_query
            .build()
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);

        let mut list_query = QueryBuilder::<MySql>::new(
            "SELECT session_id, user_id, agent_id, title, status, event_count, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at, \
             DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s') AS ended_at, \
             IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json \
             FROM agent_sessions WHERE user_id = ",
        );
        list_query.push_bind(&filter.user_id);
        if let Some(agent_id) = &filter.agent_id {
            list_query.push(" AND agent_id = ");
            list_query.push_bind(agent_id);
        }
        if let Some(status) = &filter.status {
            list_query.push(" AND status = ");
            list_query.push_bind(status);
        }
        list_query.push(" ORDER BY created_at DESC LIMIT ");
        list_query.push_bind(i64::from(filter.limit));
        list_query.push(" OFFSET ");
        list_query.push_bind(i64::from(filter.offset));

        let rows = list_query
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            sessions.push(session_record_from_row(row)?);
        }

        Ok(SessionListRecord {
            sessions,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let session = self
            .fetch_session_by_id(&pool, &session_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        if session.user_id != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("无权限访问 Session {session_id}"),
            ));
        }

        Ok(session)
    }

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let existing = self
            .fetch_session_by_id(&pool, &session_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        if existing.user_id != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("无权限修改 Session {session_id}"),
            ));
        }

        let SessionUpdateRequestData {
            title,
            metadata,
            status,
        } = request;

        if title.is_none() && metadata.is_none() && status.is_none() {
            return Ok(existing);
        }

        let mut update_query =
            QueryBuilder::<MySql>::new("UPDATE agent_sessions SET updated_at = NOW()");
        if let Some(title) = &title {
            update_query.push(", title = ");
            update_query.push_bind(title);
        }
        if let Some(metadata) = &metadata {
            update_query.push(", `metadata` = ");
            update_query.push_bind(serde_json::Value::Object(metadata.clone()).to_string());
        }
        if let Some(status) = &status {
            update_query.push(", status = ");
            update_query.push_bind(status);
            if status == "ended" {
                update_query.push(", ended_at = NOW()");
            }
        }
        update_query.push(" WHERE session_id = ");
        update_query.push_bind(&session_id);

        update_query
            .build()
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        let updated = self
            .fetch_session_by_id(&pool, &session_id)
            .await?
            .ok_or_else(|| internal_error("failed to read updated session"))?;
        let details = serde_json::json!({
            "title": title,
            "status": status,
            "metadata": metadata,
        });
        log_session_audit(&pool, &user_id, "session_update", &session_id, details).await;
        Ok(updated)
    }

    async fn delete_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let existing = self
            .fetch_session_by_id(&pool, &session_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        if existing.user_id != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("无权限删除 Session {session_id}"),
            ));
        }

        query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        let details = serde_json::json!({ "title": existing.title });
        log_session_audit(&pool, &user_id, "session_delete", &session_id, details).await;
        Ok(())
    }

    async fn get_session_activity(
        &self,
        session_id: String,
        _user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let count_row = query(
            "SELECT COUNT(*) as cnt FROM auth_audit_logs \
             WHERE resource_type = 'session' AND resource_id = ?",
        )
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        let rows = query(
            "SELECT log_id, action, details, created_at FROM auth_audit_logs \
             WHERE resource_type = 'session' AND resource_id = ? \
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(&session_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let activities = rows
            .iter()
            .map(|row| {
                let details_str: String = row.try_get("details").unwrap_or_default();
                let details = serde_json::from_str(&details_str).unwrap_or(serde_json::Value::Null);
                SessionActivityEntryRecord {
                    log_id: row.try_get("log_id").unwrap_or_default(),
                    action: row.try_get("action").unwrap_or_default(),
                    details,
                    created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
                }
            })
            .collect();

        Ok(SessionActivityRecord {
            session_id,
            activities,
            total,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredSessionService;

#[async_trait]
impl SessionService for UnconfiguredSessionService {
    async fn create_session(
        &self,
        _user_id: String,
        _request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn get_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn update_session(
        &self,
        _session_id: String,
        _user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }
}
