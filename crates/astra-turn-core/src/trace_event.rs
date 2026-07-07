use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical in-process trace event for DB-backed Web session traceability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub event_id: String,
    pub session_id: String,
    pub user_id: String,
    pub event_type: String,
    pub trace_kind: String,
    pub turn_id: Option<String>,
    pub turn_seq: Option<i64>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub round_index: Option<i64>,
    pub tool_call_id: Option<String>,
    pub meta_tool_name: Option<String>,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub token_usage: Option<serde_json::Value>,
    pub llm_model_used: Option<String>,
    pub meta_duration_ms: Option<i32>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    /// Canonical UUID v7 from the originating EventLog event, enabling
    /// cross-layer correlation with StepRecorder and EventLog.
    pub canonical_event_id: Option<Uuid>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl TraceEvent {
    pub fn new(
        event_id: impl Into<String>,
        session_id: impl Into<String>,
        user_id: impl Into<String>,
        event_type: impl Into<String>,
        trace_kind: impl Into<String>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            session_id: session_id.into(),
            user_id: user_id.into(),
            event_type: event_type.into(),
            trace_kind: trace_kind.into(),
            turn_id: None,
            turn_seq: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            parent_agent_id: None,
            round_index: None,
            tool_call_id: None,
            meta_tool_name: None,
            content: None,
            reasoning_content: None,
            token_usage: None,
            llm_model_used: None,
            meta_duration_ms: None,
            parent_event_id: None,
            causal_chain_id: None,
            canonical_event_id: None,
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
        }
    }

    pub fn with_turn_context(mut self, context: &TraceContext) -> Self {
        self.session_id = context.session_id.clone();
        self.user_id = context.user_id.clone();
        self.turn_id = Some(context.turn_id.clone());
        self.turn_seq = Some(context.turn_seq);
        self.causal_chain_id = Some(context.causal_chain_id.clone());
        self
    }
}

/// Trace identity shared by every event caused by one Web user input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub session_id: String,
    pub user_id: String,
    pub turn_id: String,
    pub turn_seq: i64,
    pub causal_chain_id: String,
    pub root_event_id: String,
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum TraceWriteError {
    #[error("trace writer is unavailable: {0}")]
    Unavailable(String),
    #[error("trace event persist failed: {0}")]
    Persist(String),
}

#[async_trait]
pub trait TraceEventWriter: Send + Sync {
    async fn write(&self, event: TraceEvent) -> Result<(), TraceWriteError>;

    async fn write_many(&self, events: Vec<TraceEvent>) -> Result<(), TraceWriteError> {
        for event in events {
            self.write(event).await?;
        }
        Ok(())
    }
}
