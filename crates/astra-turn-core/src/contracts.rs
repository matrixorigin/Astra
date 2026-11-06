use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnObserverRequest {
    pub user_id: String,
    pub session_id: String,
    pub messages: Vec<serde_json::Map<String, serde_json::Value>>,
    pub turn_count: i64,
    pub session_start: Option<serde_json::Value>,
}

/// Only `Observed` confirms that the memory backend accepted an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnObserverOutcome {
    Observed,
    NotConfigured,
    AccessDisabled,
    Empty,
}

#[async_trait]
pub trait TurnObserverWorker: Send + Sync {
    /// Deployment configuration only; user authority is resolved afresh by run.
    fn is_configured(&self) -> bool {
        true
    }

    async fn run(&self, request: TurnObserverRequest) -> Result<TurnObserverOutcome, String>;
}

#[async_trait]
pub trait TurnAuxiliaryEventWriter: Send + Sync {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnAuxiliaryEventRecord {
    pub event_id: String,
    pub user_id: String,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub content: String,
    pub parent_event_id: Option<String>,
    pub parent_event_ids: Vec<String>,
    pub causal_chain_id: String,
    pub metadata: Option<serde_json::Value>,
    pub reasoning_content: Option<String>,
}
