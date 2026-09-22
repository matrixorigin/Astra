use async_trait::async_trait;

use crate::activity::SessionActivityUpdatePlan;
use crate::hook_plans::SnapshotLinkPlan;

#[async_trait]
pub trait TurnSessionActivityWriter: Send + Sync {
    async fn update_session_activity(
        &self,
        session_id: &str,
        user_id: &str,
        plan: SessionActivityUpdatePlan,
    ) -> Result<(), String>;
}

#[async_trait]
pub trait TurnCoreEventWriter: Send + Sync {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnCoreEventRecord {
    pub event_id: String,
    pub user_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub content: String,
    pub parent_event_id: Option<String>,
    pub parent_event_ids: Vec<String>,
    pub causal_chain_id: String,
    pub turn_seq: Option<i64>,
    pub llm_model_used: Option<String>,
    pub token_usage: Option<serde_json::Value>,
    pub llm_params: Option<serde_json::Value>,
    pub reasoning_content: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnCorePersistPlan {
    pub user_query_event: Option<TurnCoreEventRecord>,
    pub llm_response_event: Option<TurnCoreEventRecord>,
    pub snapshot_link_plan: Option<SnapshotLinkPlan>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnToolEventRecord {
    pub event_id: String,
    pub user_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub content: String,
    pub parent_event_id: Option<String>,
    pub parent_event_ids: Vec<String>,
    pub causal_chain_id: String,
    pub metadata: Option<serde_json::Value>,
    pub skill_name: Option<String>,
    pub skill_version: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnToolEventPersistPlan {
    pub events: Vec<TurnToolEventRecord>,
}

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
pub struct TurnReflectionMark {
    pub session_id: String,
    pub reflect_output: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnReflectionLessonRecord {
    pub user_id: String,
    pub session_id: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnObserverRequest {
    pub user_id: String,
    pub session_id: String,
    pub messages: Vec<serde_json::Map<String, serde_json::Value>>,
    pub turn_count: i64,
    pub session_start: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnReflectionLessonRequest {
    pub user_id: String,
    pub session_id: String,
    pub retry_names: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnHookDbPersistPlan {
    pub skill_selection: Option<TurnSkillSelectionRecord>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnCorePersistOutcome {
    pub llm_response_event_id: Option<String>,
}

#[async_trait]
pub trait TurnToolEventWriter: Send + Sync {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String>;
}

#[async_trait]
pub trait TurnHookDbWriter: Send + Sync {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String>;
}

#[async_trait]
pub trait TurnReflectionStateStore: Send + Sync {
    async fn mark_reflecting(&self, mark: TurnReflectionMark) -> Result<(), String>;
    async fn pop_reflecting(&self, session_id: &str) -> Result<Option<TurnReflectionMark>, String>;
}

#[async_trait]
pub trait TurnReflectionLessonWriter: Send + Sync {
    async fn persist_lesson(&self, lesson: TurnReflectionLessonRecord) -> Result<(), String>;
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
