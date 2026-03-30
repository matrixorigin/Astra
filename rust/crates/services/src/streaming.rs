use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

use mo_agent_core::{ErrorResponse, internal_error};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamChatResponse {
    pub status: String,
    pub message: String,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait StreamingService: Send + Sync {
    async fn stream_chat(
        &self,
        user_id: String,
        request: StreamChatRequestData,
    ) -> Result<StreamChatResponse, (StatusCode, Json<ErrorResponse>)>;
}

// ── Internal request data ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct StreamChatRequestData {
    pub session_id: String,
    pub message: String,
    pub context: Option<serde_json::Value>,
    pub max_candidates: Option<i64>,
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredStreamingService;

#[async_trait]
impl StreamingService for UnconfiguredStreamingService {
    async fn stream_chat(
        &self,
        _: String,
        _: StreamChatRequestData,
    ) -> Result<StreamChatResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("streaming service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StreamChatRequest {
    pub session_id: String,
    pub message: String,
    pub context: Option<serde_json::Value>,
    pub max_candidates: Option<i64>,
}
