use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnSkillSelectionRecord {
    pub event_id: String,
    pub session_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub user_query: String,
    pub selected_skills: Vec<String>,
    pub skill_name: String,
    pub skill_version: Option<String>,
    pub selection_method: String,
    pub execution_success: Option<i64>,
    pub execution_time_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnObserverRequest {
    pub user_id: String,
    pub session_id: String,
    pub messages: Vec<serde_json::Map<String, serde_json::Value>>,
    pub turn_count: i64,
    pub session_start: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnHookDbPersistPlan {
    pub skill_selection: Option<TurnSkillSelectionRecord>,
}

#[async_trait]
pub trait TurnHookDbWriter: Send + Sync {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String>;
}

#[async_trait]
pub trait TurnObserverWorker: Send + Sync {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String>;
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
