use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct TriggerCreateRequestData {
    pub trigger_type: String,
    pub name: String,
    pub agent_id: String,
    pub user_input: String,
    pub context: Option<serde_json::Value>,
    pub cron_expr: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TriggerRecord {
    pub trigger_id: String,
    pub user_id: String,
    pub agent_id: String,
    pub trigger_type: String,
    pub name: String,
    pub user_input: String,
    pub context: Option<serde_json::Value>,
    pub cron_expr: Option<String>,
    pub session_id: Option<String>,
    pub is_active: bool,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WebhookFireData {
    pub secret: String,
    pub payload: Option<serde_json::Value>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait TriggerService: Send + Sync {
    async fn create_trigger(
        &self,
        user_id: String,
        request: TriggerCreateRequestData,
    ) -> Result<TriggerRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_triggers(
        &self,
        user_id: String,
    ) -> Result<Vec<TriggerRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_trigger(
        &self,
        trigger_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn fire_webhook(
        &self,
        trigger_id: String,
        request: WebhookFireData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseTriggerService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseTriggerService {
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
impl TriggerService for DatabaseTriggerService {
    async fn create_trigger(
        &self,
        user_id: String,
        request: TriggerCreateRequestData,
    ) -> Result<TriggerRecord, (StatusCode, Json<ErrorResponse>)> {
        if request.trigger_type != "webhook" && request.trigger_type != "schedule" {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "trigger_type must be 'webhook' or 'schedule'",
            ));
        }
        if request.trigger_type == "schedule" && request.cron_expr.is_none() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "cron_expr required for schedule triggers",
            ));
        }

        let pool = self.get_pool().await.map_err(internal_error)?;
        let trigger_id = Uuid::new_v4().to_string();

        let secret = if request.trigger_type == "webhook" {
            Some(Uuid::new_v4().to_string())
        } else {
            None
        };

        let context_json = request
            .context
            .as_ref()
            .map(|c| serde_json::to_string(c).unwrap_or_else(|_| "null".into()));

        query(
            "INSERT INTO wf_triggers \
             (trigger_id, user_id, agent_id, trigger_type, name, user_input, context, \
              cron_expr, secret, session_id, is_active, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, NOW())",
        )
        .bind(&trigger_id)
        .bind(&user_id)
        .bind(&request.agent_id)
        .bind(&request.trigger_type)
        .bind(&request.name)
        .bind(&request.user_input)
        .bind(&context_json)
        .bind(&request.cron_expr)
        .bind(&secret)
        .bind(&request.session_id)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(TriggerRecord {
            trigger_id,
            user_id,
            agent_id: request.agent_id,
            trigger_type: request.trigger_type,
            name: request.name,
            user_input: request.user_input,
            context: request.context,
            cron_expr: request.cron_expr,
            session_id: request.session_id,
            is_active: true,
            created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            secret,
        })
    }

    async fn list_triggers(
        &self,
        user_id: String,
    ) -> Result<Vec<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let rows = query(
            "SELECT trigger_id, user_id, agent_id, trigger_type, name, user_input, \
             IFNULL(CAST(context AS CHAR), 'null') AS context_json, \
             cron_expr, session_id, is_active, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM wf_triggers WHERE user_id = ? AND is_active = 1 ORDER BY created_at DESC",
        )
        .bind(&user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut triggers = Vec::with_capacity(rows.len());
        for row in rows {
            let ctx_json: String = row
                .try_get("context_json")
                .unwrap_or_else(|_| "null".into());
            triggers.push(TriggerRecord {
                trigger_id: row.try_get("trigger_id").map_err(internal_error)?,
                user_id: row.try_get("user_id").map_err(internal_error)?,
                agent_id: row.try_get("agent_id").map_err(internal_error)?,
                trigger_type: row.try_get("trigger_type").map_err(internal_error)?,
                name: row.try_get("name").map_err(internal_error)?,
                user_input: row.try_get("user_input").map_err(internal_error)?,
                context: serde_json::from_str(&ctx_json).ok(),
                cron_expr: row.try_get("cron_expr").ok(),
                session_id: row.try_get("session_id").ok(),
                is_active: row.try_get::<i16, _>("is_active").unwrap_or(1) != 0,
                created_at: row.try_get("created_at").unwrap_or_default(),
                secret: None, // Never expose secret in list
            });
        }
        Ok(triggers)
    }

    async fn delete_trigger(
        &self,
        trigger_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query("SELECT user_id FROM wf_triggers WHERE trigger_id = ?")
            .bind(&trigger_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Trigger not found"))?;
        let owner: String = row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Not authorized"));
        }

        query("DELETE FROM wf_triggers WHERE trigger_id = ?")
            .bind(&trigger_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        Ok(())
    }

    async fn fire_webhook(
        &self,
        trigger_id: String,
        request: WebhookFireData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT trigger_type, secret, user_input, agent_id, session_id FROM wf_triggers WHERE trigger_id = ?"
        )
        .bind(&trigger_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;
        let row = row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Trigger not found"))?;

        let trigger_type: String = row.try_get("trigger_type").map_err(internal_error)?;
        if trigger_type != "webhook" {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Not a webhook trigger",
            ));
        }

        let stored_secret: Option<String> = row.try_get("secret").ok();
        match stored_secret {
            Some(s) if s == request.secret => {}
            _ => return Err(error_response(StatusCode::FORBIDDEN, "Invalid secret")),
        }

        Ok(serde_json::json!({
            "trigger_id": trigger_id,
            "fired": true,
            "payload": request.payload,
        }))
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredTriggerService;

#[async_trait]
impl TriggerService for UnconfiguredTriggerService {
    async fn create_trigger(
        &self,
        _: String,
        _: TriggerCreateRequestData,
    ) -> Result<TriggerRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("trigger service not configured"))
    }
    async fn list_triggers(
        &self,
        _: String,
    ) -> Result<Vec<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("trigger service not configured"))
    }
    async fn delete_trigger(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("trigger service not configured"))
    }
    async fn fire_webhook(
        &self,
        _: String,
        _: WebhookFireData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("trigger service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTriggerRequest {
    pub trigger_type: String,
    pub name: String,
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
    pub user_input: String,
    pub context: Option<serde_json::Value>,
    pub cron_expr: Option<String>,
    pub session_id: Option<String>,
}

fn default_agent_id() -> String {
    "dev-agent".to_string()
}

#[derive(Deserialize)]
pub struct WebhookFireRequest {
    pub secret: String,
    pub payload: Option<serde_json::Value>,
}
