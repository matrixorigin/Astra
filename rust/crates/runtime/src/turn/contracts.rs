use astra_services::MutationObjectiveScore;

use crate::*;

#[async_trait]
pub trait TurnSessionActivityWriter: Send + Sync {
    async fn update_session_activity(
        &self,
        session_id: &str,
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
    pub agent_id: Option<String>,
    pub event_type: String,
    pub content: String,
    pub parent_event_id: Option<String>,
    pub parent_event_ids: Vec<String>,
    pub causal_chain_id: String,
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
pub struct TurnDecisionAuditRecord {
    pub decision_id: String,
    pub session_id: String,
    pub event_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_used: Option<String>,
    pub context_capture_id: Option<String>,
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
pub struct TurnImplicitFeedbackRecord {
    pub feedback_id: String,
    pub prompt_template_id: String,
    pub prompt_version: String,
    pub llm_request_id: String,
    pub rating: i64,
    pub comment: Option<String>,
    pub metadata: Option<serde_json::Value>,
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
pub(crate) struct TurnReflectionLessonRequest {
    pub user_id: String,
    pub session_id: String,
    pub retry_names: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnHookDbPersistPlan {
    pub decision_audit: Option<TurnDecisionAuditRecord>,
    pub skill_selection: Option<TurnSkillSelectionRecord>,
    pub implicit_feedback: Option<TurnImplicitFeedbackRecord>,
    pub reflection_mark: Option<TurnReflectionMark>,
    pub reflection_lesson: Option<TurnReflectionLessonRecord>,
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

// ─── Pipeline Learning ──────────────────────────────────────────────────────────

/// Outcome of a completed turn, used to update pipeline learning modules
/// (EntityGraph, PatternLibrary, ProgressiveCalibrator).
#[derive(Clone, Debug)]
pub struct TurnLearningOutcome {
    /// The user's query text for this turn.
    pub query: String,
    /// Tool names that were selected for the LLM.
    pub tools_selected: Vec<String>,
    /// Tool names actually invoked by the LLM.
    pub tools_used: Vec<String>,
    /// Whether the turn completed successfully (no errors, tools ran).
    pub success: bool,
    /// Aggregate quality score (0.0–1.0), derived from tool quality assessments.
    pub quality: f64,
    /// Whether the user corrected the agent's behavior in a follow-up.
    pub was_corrected: bool,
    /// Routing metadata: task_type label from RoutingDecision.
    pub task_type_label: Option<String>,
    /// Routing metadata: domain_hint label from RoutingDecision.
    pub domain_hint_label: Option<String>,
    /// User feedback score (0-100), pulled from skill_selection_events.
    /// Used to close the feedback loop: low scores indicate user dissatisfaction
    /// even if the turn technically succeeded.
    pub user_feedback_score: Option<i64>,
    /// Risk that the turn's apparent success came from repetitive cheap actions
    /// rather than genuine progress.
    pub reward_hacking_risk: f64,
    /// Human-readable reasons for the reward-hacking risk score.
    pub reward_hacking_flags: Vec<String>,
    /// Confidence that the turn has corroborating causal support from tool
    /// evidence rather than a spurious success correlation.
    pub causal_support_score: f64,
    /// Human-readable reasons for weakened causal support.
    pub causal_support_flags: Vec<String>,
}

impl TurnLearningOutcome {
    pub fn mutation_objective_score(&self) -> MutationObjectiveScore {
        MutationObjectiveScore::from_learning_signal(
            self.quality,
            self.user_feedback_score,
            self.reward_hacking_risk,
            self.causal_support_score,
            self.was_corrected,
        )
    }
}

/// Trait for recording turn outcomes into pipeline learning modules.
/// Implementations update EntityGraph, PatternLibrary, and ProgressiveCalibrator.
#[async_trait]
pub trait TurnLearningWriter: Send + Sync {
    async fn record_outcome(&self, outcome: TurnLearningOutcome) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_event_persist_plan_default_empty() {
        let plan = TurnToolEventPersistPlan::default();
        assert!(plan.events.is_empty());
    }

    #[test]
    fn hook_db_persist_plan_default_all_none() {
        let plan = TurnHookDbPersistPlan::default();
        assert!(plan.decision_audit.is_none());
        assert!(plan.skill_selection.is_none());
        assert!(plan.implicit_feedback.is_none());
        assert!(plan.reflection_mark.is_none());
        assert!(plan.reflection_lesson.is_none());
    }

    #[test]
    fn core_persist_outcome_default_none() {
        let outcome = TurnCorePersistOutcome::default();
        assert!(outcome.llm_response_event_id.is_none());
    }

    #[test]
    fn core_event_record_carries_agent_id() {
        let record = TurnCoreEventRecord {
            event_id: "e1".into(),
            user_id: "u1".into(),
            session_id: "s1".into(),
            agent_id: Some("astra-cli".into()),
            event_type: "user_query".into(),
            content: "hi".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: "c1".into(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        };
        assert_eq!(record.agent_id.as_deref(), Some("astra-cli"));
    }

    #[test]
    fn core_event_record_agent_id_none() {
        let record = TurnCoreEventRecord {
            event_id: "e1".into(),
            user_id: "u1".into(),
            session_id: "s1".into(),
            agent_id: None,
            event_type: "user_query".into(),
            content: "hi".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: "c1".into(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        };
        assert!(record.agent_id.is_none());
    }

    #[test]
    fn tool_event_record_carries_agent_id() {
        let record = TurnToolEventRecord {
            event_id: "e1".into(),
            user_id: "u1".into(),
            session_id: "s1".into(),
            agent_id: Some("custom-agent".into()),
            event_type: "tool_call".into(),
            content: "{}".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: "c1".into(),
            metadata: None,
            skill_name: None,
            skill_version: None,
            reasoning_content: None,
        };
        assert_eq!(record.agent_id.as_deref(), Some("custom-agent"));
    }

    #[test]
    fn auxiliary_event_record_carries_agent_id() {
        let record = TurnAuxiliaryEventRecord {
            event_id: "e1".into(),
            user_id: "u1".into(),
            session_id: "s1".into(),
            agent_id: Some("astra-cli".into()),
            event_type: "routing_decision".into(),
            content: "{}".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: "c1".into(),
            metadata: None,
            reasoning_content: None,
        };
        assert_eq!(record.agent_id.as_deref(), Some("astra-cli"));
    }

    #[test]
    fn turn_learning_outcome_exposes_mutation_objective_score() {
        let outcome = TurnLearningOutcome {
            query: "update the migration note".into(),
            tools_selected: vec!["write_file".into()],
            tools_used: vec!["write_file".into()],
            success: true,
            quality: 0.82,
            was_corrected: false,
            task_type_label: Some("mutate".into()),
            domain_hint_label: Some("code".into()),
            user_feedback_score: Some(90),
            reward_hacking_risk: 0.15,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 0.78,
            causal_support_flags: Vec::new(),
        };

        let scoreboard = outcome.mutation_objective_score();
        assert_eq!(scoreboard.quality.point, 0.82);
        assert_eq!(scoreboard.user_feedback.map(|value| value.point), Some(0.9));
        assert_eq!(scoreboard.reward_hacking_risk.point, 0.15);
        assert_eq!(scoreboard.causal_support.point, 0.78);
        assert!(!scoreboard.was_corrected);
    }
}
